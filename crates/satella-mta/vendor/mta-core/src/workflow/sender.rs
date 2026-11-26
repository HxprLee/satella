//! Sender workflow
//!
//! High-level API wrapping the complete send flow:
//! 1. Create the Wi-Fi P2P hotspot
//! 2. Start the HTTP transfer server
//! 3. Connect to the receiver over BLE and send the P2P info
//! 4. Wait for the receiver to connect and download the files

use crate::ble::{BleClient, DiscoveredDevice};
use crate::crypto::BleSecurityPersistent;
use crate::transfer::{FileEntry, TransferServer, TransferTask};
use crate::wifi::{P2pConfig, WiFiP2pSender};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Send progress callback
pub trait SendProgressCallback: Send + Sync {
    /// Status update
    fn on_status(&self, status: &str);
    /// Progress update
    fn on_progress(&self, sent: u64, total: u64);
    /// Send complete
    fn on_complete(&self);
    /// Send failed
    fn on_error(&self, error: &str);
}

/// Send options
pub struct SendOptions {
    /// Wi-Fi interface name
    pub wifi_interface: String,
    /// Whether to use 5GHz
    pub use_5ghz: bool,
    /// Sender name
    pub sender_name: String,
    /// Path where the persistent identity is stored. `None` means no
    /// persistence.
    pub identity_path: Option<PathBuf>,
}

impl Default for SendOptions {
    fn default() -> Self {
        Self {
            wifi_interface: "wlan0".to_string(),
            use_5ghz: true,
            sender_name: hostname::get()
                .map(|h| h.to_string_lossy().to_string())
                .unwrap_or_else(|_| "Cattysend".to_string()),
            identity_path: None,
        }
    }
}

/// Sender workflow
pub struct Sender {
    options: SendOptions,
    wifi_sender: WiFiP2pSender,
    security: Arc<BleSecurityPersistent>,
}

impl Sender {
    pub fn new(options: SendOptions) -> anyhow::Result<Self> {
        let wifi_sender = WiFiP2pSender::with_config(P2pConfig {
            interface: options.wifi_interface.clone(),
            use_5ghz: options.use_5ghz,
            ..Default::default()
        });

        let security = match &options.identity_path {
            Some(path) => Arc::new(BleSecurityPersistent::load_or_create(path)?),
            None => Arc::new(BleSecurityPersistent::new()?),
        };

        Ok(Self {
            options,
            wifi_sender,
            security,
        })
    }

    /// Sends files to the specified device
    pub async fn send_to_device<C: SendProgressCallback>(
        &self,
        device: &DiscoveredDevice,
        files: Vec<PathBuf>,
        callback: &C,
    ) -> anyhow::Result<()> {
        callback.on_status("Preparing to send...");

        // Prepare the file information
        let mut file_entries = Vec::new();
        let mut _total_size: u64 = 0;

        for path in &files {
            let metadata = tokio::fs::metadata(path).await?;
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let size = metadata.len();
            _total_size += size;

            // Guess the MIME type
            let mime_type = mime_guess::from_path(path)
                .first()
                .map(|m| m.to_string())
                .unwrap_or_else(|| "application/octet-stream".to_string());

            file_entries.push(FileEntry {
                path: path.clone(),
                name,
                size,
                mime_type,
            });
        }

        callback.on_status("Creating Wi-Fi hotspot...");

        // Create the transfer task
        let task_id = uuid::Uuid::new_v4().to_string();
        let sender_id = format!("{:04x}", rand::random::<u16>());

        let task = TransferTask {
            task_id: task_id.clone(),
            files: file_entries,
            sender_id: sender_id.clone(),
            sender_name: self.options.sender_name.clone(),
        };

        // Start the transfer server
        //
        // The receiver connects over `wss://` and `https://`, so this must be
        // TLS.
        let mut server = TransferServer::new(task);
        let port = server.start_tls().await?;

        callback.on_status(&format!("Server started on port {}", port));

        // Create the Wi-Fi P2P hotspot
        let p2p_info = self.wifi_sender.create_group(port as i32).await?;

        callback.on_status(&format!("Hotspot created: {}", p2p_info.ssid));

        // Connect to the receiver's BLE device
        // Every step after the hotspot is up can fail; the hotspot and DHCP
        // have to be torn down either way, otherwise the next send runs into
        // a leftover group.
        let transfer = async {
            callback.on_status("Connecting to receiver...");

            let ble_client = BleClient::new().await?.with_security(self.security.clone());
            let _device_info = ble_client
                .connect_and_handshake_named(
                    &device.address,
                    Some(&device.name),
                    &p2p_info,
                    &sender_id,
                )
                .await?;

            callback.on_status("Waiting for the receiver to connect...");

            // Subscribe to the transfer status
            let mut status_rx = server.subscribe_status_async().await;

            // Wait for the transfer to complete or time out
            let timeout = std::time::Duration::from_secs(300); // 5 minute timeout
            let result = tokio::time::timeout(timeout, async {
                loop {
                    match status_rx.recv().await {
                        Ok(crate::transfer::TransferStatus::Completed) => {
                            return Ok(());
                        }
                        Ok(crate::transfer::TransferStatus::Rejected(reason)) => {
                            return Err(anyhow::anyhow!("The receiver refused: {}", reason));
                        }
                        Ok(crate::transfer::TransferStatus::Transferring { progress }) => {
                            let percent = (progress * 100.0) as u64;
                            callback.on_progress(percent, 100);
                        }
                        Ok(crate::transfer::TransferStatus::Failed(e)) => {
                            return Err(anyhow::anyhow!("Transfer failed: {}", e));
                        }
                        Err(e) => {
                            // Channel closed, possibly because the server stopped
                            return Err(anyhow::anyhow!("Status channel error: {}", e));
                        }
                        _ => {}
                    }
                }
            })
            .await;

            match result {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!("Transfer timed out")),
            }
        }
        .await;

        if let Err(err) = self.wifi_sender.stop_group().await {
            log::warn!("Could not stop the P2P group: {err}");
        }

        match transfer {
            Ok(()) => {
                callback.on_status("Transfer complete!");
                callback.on_complete();
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

/// Simplified send callback implementation
pub struct SimpleSendCallback {
    tx: mpsc::Sender<SendEvent>,
}

#[derive(Debug, Clone)]
pub enum SendEvent {
    Status(String),
    Progress { sent: u64, total: u64 },
    Complete,
    Error(String),
}

impl SimpleSendCallback {
    pub fn new() -> (Self, mpsc::Receiver<SendEvent>) {
        let (tx, rx) = mpsc::channel(32);
        (Self { tx }, rx)
    }
}

impl SendProgressCallback for SimpleSendCallback {
    fn on_status(&self, status: &str) {
        let _ = self.tx.try_send(SendEvent::Status(status.to_string()));
    }

    fn on_progress(&self, sent: u64, total: u64) {
        let _ = self.tx.try_send(SendEvent::Progress { sent, total });
    }

    fn on_complete(&self) {
        let _ = self.tx.try_send(SendEvent::Complete);
    }

    fn on_error(&self, error: &str) {
        let _ = self.tx.try_send(SendEvent::Error(error.to_string()));
    }
}
