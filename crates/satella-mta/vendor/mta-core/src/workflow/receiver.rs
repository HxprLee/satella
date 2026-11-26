//! Receiver workflow
//!
//! High-level API wrapping the complete receive flow:
//! 1. Start the GATT server and wait for a connection
//! 2. Receive the P2P info
//! 3. Connect to the sender's Wi-Fi hotspot
//! 4. Receive files over HTTP/WebSocket

use crate::ble::GattServer;
use crate::crypto::BleSecurityPersistent;
use crate::transfer::{ReceiverCallback, ReceiverClient, SendRequest};
use crate::wifi::WiFiP2pReceiver;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

/// Receive progress callback
#[async_trait]
pub trait ReceiveProgressCallback: Send + Sync {
    /// Status update
    fn on_status(&self, status: &str);
    /// Called on a send request; returns whether to accept it
    ///
    /// The answer may take a while: the sender waits for it, so an
    /// implementation that asks the user can wait for the user.
    async fn on_request(&self, request: &ReceiveRequest) -> bool;
    /// Progress update
    fn on_progress(&self, received: u64, total: u64);
    /// Receive complete
    fn on_complete(&self, files: Vec<PathBuf>);
    /// Receive failed
    fn on_error(&self, error: &str);
}

/// Receive request information
#[derive(Debug, Clone)]
pub struct ReceiveRequest {
    pub sender_name: String,
    pub file_name: String,
    pub file_count: u32,
    pub total_size: u64,
    /// The sender's task ID, unique for every transfer.
    ///
    /// It is used to match the sender's request with the application's answer.
    pub task_id: String,
}

/// Receive options
pub struct ReceiveOptions {
    /// Device name (shown in BLE advertising)
    pub device_name: String,
    /// Wi-Fi interface name
    pub wifi_interface: String,
    /// Directory where files are saved
    pub output_dir: PathBuf,
    /// Whether to auto-accept
    pub auto_accept: bool,
    /// Vendor ID
    pub brand_id: crate::config::BrandId,
    /// Whether 5GHz is supported
    pub supports_5ghz: bool,
    /// Path where the persistent identity is stored.
    ///
    /// The identity (ECDH key pair and sender ID) has to stay the same across
    /// restarts, otherwise the peer treats this device as a new one. `None`
    /// means no persistence.
    pub identity_path: Option<PathBuf>,
}

impl Default for ReceiveOptions {
    fn default() -> Self {
        Self {
            device_name: hostname::get()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_else(|_| "Cattysend".to_string()),
            wifi_interface: "wlan0".to_string(),
            output_dir: dirs::download_dir().unwrap_or_else(|| PathBuf::from(".")),
            auto_accept: false,
            brand_id: crate::config::BrandId::Xiaomi,
            supports_5ghz: true,
            identity_path: None,
        }
    }
}

/// Receiver workflow
pub struct Receiver {
    options: ReceiveOptions,
    security: Arc<BleSecurityPersistent>,
}

impl Receiver {
    pub fn new(options: ReceiveOptions) -> anyhow::Result<Self> {
        // The identity has to stay the same across restarts, so load it from
        // the file and only create a new one when there is no file.
        let security = match &options.identity_path {
            Some(path) => Arc::new(BleSecurityPersistent::load_or_create(path)?),
            None => Arc::new(BleSecurityPersistent::new()?),
        };
        Ok(Self { options, security })
    }

    /// Starts receive mode
    pub async fn start<C: ReceiveProgressCallback>(
        &self,
        callback: &C,
    ) -> anyhow::Result<Vec<PathBuf>> {
        callback.on_status("Starting receiver mode...");

        // Get the MAC address
        let mac = self.get_mac_address();

        // Start the GATT server
        let mut gatt_server = GattServer::new(
            mac,
            self.options.device_name.clone(),
            self.security.get_public_key().to_string(),
        )?
        .with_security(self.security.clone())
        .with_brand(self.options.brand_id)
        .with_5ghz_support(self.options.supports_5ghz);
        let mut p2p_rx = gatt_server.take_p2p_receiver().unwrap();

        let _handle = gatt_server.start().await?;

        callback.on_status(&format!(
            "Advertising as '{}', waiting for a sender...",
            self.options.device_name
        ));

        // Wait for the P2P info
        let p2p_event = p2p_rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("P2P channel closed"))?;

        // The P2P info has already been decrypted by the GattServer (when a
        // public key was provided)
        let p2p_info = p2p_event.p2p_info;

        if p2p_event.sender_public_key.is_some() {
            callback.on_status("Received and decrypted P2P info");
        } else {
            callback.on_status("Received P2P info");
        }

        callback.on_status(&format!("Connecting to Wi-Fi: {}", p2p_info.ssid));

        // Connect to the Wi-Fi P2P hotspot (dual connection supported)
        let mut wifi_receiver = WiFiP2pReceiver::new(&self.options.wifi_interface);
        let local_ip = wifi_receiver.connect(&p2p_info).await?;

        // Show the connection status
        if wifi_receiver.is_dual_connected().await {
            callback.on_status(&format!(
                "✅ Connected (dual-connection mode), local IP: {}",
                local_ip
            ));
        } else {
            callback.on_status(&format!("✅ Connected, local IP: {}", local_ip));
        }

        // Compute the sender IP (usually the gateway)
        let sender_ip = self.get_gateway_ip(&local_ip);

        callback.on_status(&format!(
            "Connecting to WebSocket: wss://{}:{}/websocket",
            sender_ip, p2p_info.port
        ));

        // Create the receiver adapter
        let adapter = ReceiverCallbackAdapter {
            callback,
            auto_accept: self.options.auto_accept,
        };

        // Receive the files
        let client = ReceiverClient::new(
            &sender_ip,
            p2p_info.port as u16,
            self.options.output_dir.clone(),
        );

        let files = client.start(&adapter).await?;

        // Disconnect Wi-Fi and clean up the virtual interface
        wifi_receiver.disconnect().await?;

        callback.on_complete(files.clone());

        Ok(files)
    }

    /// Returns the MAC address
    fn get_mac_address(&self) -> String {
        let path = format!("/sys/class/net/{}/address", self.options.wifi_interface);
        std::fs::read_to_string(&path)
            .map(|s| s.trim().to_uppercase())
            .unwrap_or_else(|_| "02:00:00:00:00:00".to_string())
    }

    /// Infers the gateway IP from the local IP
    fn get_gateway_ip(&self, local_ip: &str) -> String {
        // The gateway is usually x.x.x.1
        let parts: Vec<&str> = local_ip.split('.').collect();
        if parts.len() == 4 {
            format!("{}.{}.{}.1", parts[0], parts[1], parts[2])
        } else {
            "192.168.49.1".to_string()
        }
    }
}

/// Receiver callback adapter
struct ReceiverCallbackAdapter<'a, C: ReceiveProgressCallback> {
    callback: &'a C,
    auto_accept: bool,
}

#[async_trait]
impl<C: ReceiveProgressCallback> ReceiverCallback for ReceiverCallbackAdapter<'_, C> {
    async fn on_send_request(&self, request: &SendRequest) -> bool {
        if self.auto_accept {
            return true;
        }

        let req = ReceiveRequest {
            sender_name: request.sender_name.clone(),
            file_name: request.file_name.clone(),
            file_count: request.file_count,
            total_size: request.total_size,
            task_id: request.get_task_id(),
        };

        self.callback.on_request(&req).await
    }

    fn on_progress(&self, received: u64, total: u64) {
        self.callback.on_progress(received, total);
    }

    fn on_complete(&self, files: Vec<PathBuf>) {
        self.callback.on_complete(files);
    }

    fn on_error(&self, error: String) {
        self.callback.on_error(&error);
    }
}

/// Simplified receive callback implementation
pub struct SimpleReceiveCallback {
    tx: mpsc::Sender<ReceiveEvent>,
    auto_accept: bool,
}

/// The sender wants to send files.
///
/// The application has to answer on `answer`, either accept or refuse. The
/// sender waits for this answer until it gives up on its own.
#[derive(Debug)]
pub enum ReceiveEvent {
    Status(String),
    Request {
        request: ReceiveRequest,
        answer: oneshot::Sender<bool>,
    },
    Progress {
        received: u64,
        total: u64,
    },
    Complete(Vec<PathBuf>),
    Error(String),
}

impl SimpleReceiveCallback {
    pub fn new(auto_accept: bool) -> (Self, mpsc::Receiver<ReceiveEvent>) {
        let (tx, rx) = mpsc::channel(32);
        (Self { tx, auto_accept }, rx)
    }
}

#[async_trait]
impl ReceiveProgressCallback for SimpleReceiveCallback {
    fn on_status(&self, status: &str) {
        let _ = self.tx.try_send(ReceiveEvent::Status(status.to_string()));
    }

    async fn on_request(&self, request: &ReceiveRequest) -> bool {
        if self.auto_accept {
            return true;
        }

        // The answer travels with the request, so the application can take its
        // time before it answers.
        let (answer_tx, answer_rx) = oneshot::channel();
        if self
            .tx
            .try_send(ReceiveEvent::Request {
                request: request.clone(),
                answer: answer_tx,
            })
            .is_err()
        {
            // Nobody is listening; the transfer cannot be answered.
            return false;
        }

        // A dropped answer means the application is gone, which is a refusal.
        answer_rx.await.unwrap_or(false)
    }

    fn on_progress(&self, received: u64, total: u64) {
        let _ = self.tx.try_send(ReceiveEvent::Progress { received, total });
    }

    fn on_complete(&self, files: Vec<PathBuf>) {
        let _ = self.tx.try_send(ReceiveEvent::Complete(files));
    }

    fn on_error(&self, error: &str) {
        let _ = self.tx.try_send(ReceiveEvent::Error(error.to_string()));
    }
}
