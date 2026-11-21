//! The Mutual Transfer Alliance adapter.
//!
//! The protocol engine is vendored from [Tinnci/cattysend] (MIT); this crate
//! only translates between that engine and Satella's protocol-neutral types.
//! See `THIRD_PARTY_NOTICES.md`.
//!
//! The MTA protocol discovers peers over Bluetooth Low Energy and transfers
//! the files over a Wi-Fi Direct group, so the adapter needs a working
//! Bluetooth adapter and a Wi-Fi interface. On Linux both are reached through
//! BlueZ and NetworkManager, without elevated privileges.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use satella_core::adapter::{
    AdapterError, AdapterEvent, AdapterResult, EventSender, ProtocolAdapter,
};
use satella_core::peer::{DeviceType, FileEntry, Peer, ReceivePending, SendFile, SendRequest};
use satella_core::Protocol;

use satella_mta_vendor::ble::{DiscoveredDevice, ScanCallback};
use satella_mta_vendor::workflow::{ReceiveEvent, ReceiveRequest, SendEvent, SendOptions, Sender};

/// How long a single BLE scan runs before it is repeated.
const SCAN_DURATION: Duration = Duration::from_secs(10);

/// The configuration of the MTA adapter.
#[derive(Debug, Clone)]
pub struct Config {
    /// The name this device shows in its BLE advertisement.
    pub device_name: String,
    /// The Wi-Fi interface the Wi-Fi Direct group is created on.
    pub wifi_interface: String,
    /// The directory received files are written into.
    pub output_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_name: "Satella Device".to_owned(),
            wifi_interface: "wlan0".to_owned(),
            output_dir: dirs::download_dir().unwrap_or_else(|| PathBuf::from(".")),
        }
    }
}

/// The MTA adapter.
pub struct MtaAdapter {
    config: Config,
    state: tokio::sync::Mutex<Option<InnerState>>,
}

struct InnerState {
    /// The peers seen in the last scan, by their BLE address.
    peers: Arc<std::sync::Mutex<HashMap<String, Peer>>>,
}

impl MtaAdapter {
    /// Builds an adapter for the given configuration.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            state: tokio::sync::Mutex::new(None),
        }
    }
}

/// Builds the peer of a discovered MTA device.
fn peer_from_device(device: &DiscoveredDevice) -> Peer {
    let mut peer = Peer::new(Protocol::Mta, device.name.clone(), device.address.clone());
    peer.device_model = Some(device.brand.clone());
    peer.device_type = Some(DeviceType::Mobile);
    peer
}

/// The callback that forwards the scanned devices to the adapter.
struct ScanForwarder {
    peers: Arc<std::sync::Mutex<HashMap<String, Peer>>>,
    events: EventSender,
}

#[async_trait]
impl ScanCallback for ScanForwarder {
    async fn on_device_found(&self, device: DiscoveredDevice) {
        let peer = peer_from_device(&device);
        self.peers
            .lock()
            .unwrap()
            .insert(peer.fingerprint.clone(), peer.clone());

        let _ = self.events.send(AdapterEvent::Updated(peer)).await;
    }
}

#[async_trait]
impl ProtocolAdapter for MtaAdapter {
    fn protocol(&self) -> Protocol {
        Protocol::Mta
    }

    async fn start(&self, events: EventSender) -> AdapterResult<()> {
        let peers = Arc::new(std::sync::Mutex::new(HashMap::new()));

        // The receiver runs in the background: it advertises this device over
        // BLE and waits for a sender to offer files.
        {
            let events = events.clone();
            let config = self.config.clone();
            tokio::spawn(async move {
                loop {
                    // The vendor's options type is not cloneable, so it is
                    // rebuilt for every attempt.
                    let options = satella_mta_vendor::workflow::ReceiveOptions {
                        device_name: config.device_name.clone(),
                        wifi_interface: config.wifi_interface.clone(),
                        output_dir: config.output_dir.clone(),
                        auto_accept: false,
                        brand_id: satella_mta_vendor::config::BrandId::Unknown,
                        supports_5ghz: true,
                    };

                    let Ok(receiver) = satella_mta_vendor::workflow::Receiver::new(options) else {
                        tracing::error!("could not start the MTA receiver");
                        return;
                    };

                    let (callback, mut event_rx) =
                        satella_mta_vendor::workflow::SimpleReceiveCallback::new(false);

                    let handle = tokio::spawn(async move { receiver.start(&callback).await });

                    while let Some(event) = event_rx.recv().await {
                        match event {
                            ReceiveEvent::Status(status) => {
                                tracing::debug!(status);
                            }
                            ReceiveEvent::Request(request) => {
                                let receive_pending = pending_from_request(&request);
                                if events
                                    .send(AdapterEvent::ReceivePending(receive_pending))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            ReceiveEvent::Progress { received, total } => {
                                let _ = events
                                    .send(AdapterEvent::Progress(
                                        satella_core::peer::TransferProgress::Bytes {
                                            transferred: received,
                                            total,
                                        },
                                    ))
                                    .await;
                            }
                            ReceiveEvent::Complete(_) => {
                                let _ = events
                                    .send(AdapterEvent::Progress(
                                        satella_core::peer::TransferProgress::Finished,
                                    ))
                                    .await;
                            }
                            ReceiveEvent::Error(reason) => {
                                let _ = events
                                    .send(AdapterEvent::Progress(
                                        satella_core::peer::TransferProgress::Failed { reason },
                                    ))
                                    .await;
                            }
                        }
                    }

                    handle.abort();
                }
            });
        }

        // The scanner runs in the background: it repeats a scan and forwards
        // every device it sees.
        {
            let peers = peers.clone();
            let events = events.clone();
            tokio::spawn(async move {
                let forwarder = Arc::new(ScanForwarder {
                    peers: peers.clone(),
                    events: events.clone(),
                });

                loop {
                    let scanner = match satella_mta_vendor::ble::BleScanner::new().await {
                        Ok(scanner) => scanner,
                        Err(err) => {
                            tracing::error!("could not open the Bluetooth adapter: {err}");
                            return;
                        }
                    };

                    if let Err(err) = scanner
                        .scan(
                            SCAN_DURATION,
                            Some(forwarder.clone() as Arc<dyn ScanCallback>),
                        )
                        .await
                    {
                        tracing::error!("the Bluetooth scan failed: {err}");
                    }
                }
            });
        }

        *self.state.lock().await = Some(InnerState { peers });

        Ok(())
    }

    async fn stop(&self) -> AdapterResult<()> {
        if let Some(state) = self.state.lock().await.take() {
            state.peers.lock().unwrap().clear();
        }
        Ok(())
    }

    fn peers(&self) -> Vec<Peer> {
        match self.state.try_lock() {
            Ok(state) => state
                .as_ref()
                .map(|s| s.peers.lock().unwrap().values().cloned().collect())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    async fn send(&self, request: SendRequest) -> AdapterResult<()> {
        let state = self.state.lock().await;
        let Some(state) = state.as_ref() else {
            return Err(AdapterError::Startup {
                protocol: Protocol::Mta,
                reason: "the adapter is not running".to_owned(),
            });
        };

        // The MTA workflow needs the device as the scanner saw it, because the
        // BLE address is how the handshake finds the receiver.
        let device = state
            .peers
            .lock()
            .unwrap()
            .get(&request.peer.fingerprint)
            .map(|peer| DiscoveredDevice {
                name: peer.alias.clone(),
                address: peer.fingerprint.clone(),
                sender_id: String::new(),
                brand: peer.device_model.clone().unwrap_or_default(),
                brand_id: None,
                rssi: None,
                supports_5ghz: true,
            });

        let Some(device) = device else {
            return Err(AdapterError::UnknownPeer {
                protocol: Protocol::Mta,
            });
        };

        // The MTA workflow transfers files from the local filesystem.
        let mut files = Vec::new();
        for file in request.files {
            let SendFile::Path(path) = file else {
                return Err(AdapterError::Transfer {
                    protocol: Protocol::Mta,
                    reason: "MTA can only send files from disk".to_owned(),
                });
            };
            files.push(path);
        }

        let sender = Sender::new(SendOptions {
            wifi_interface: self.config.wifi_interface.clone(),
            use_5ghz: true,
            sender_name: self.config.device_name.clone(),
        })
        .map_err(|err| AdapterError::Startup {
            protocol: Protocol::Mta,
            reason: err.to_string(),
        })?;

        let (callback, mut event_rx) = satella_mta_vendor::workflow::SimpleSendCallback::new();

        let handle =
            tokio::spawn(async move { sender.send_to_device(&device, files, &callback).await });

        let mut outcome = Ok(());
        while let Some(event) = event_rx.recv().await {
            match event {
                SendEvent::Status(status) => tracing::debug!(status),
                SendEvent::Progress { sent, total } => {
                    let _ = sent;
                    let _ = total;
                }
                SendEvent::Complete => break,
                SendEvent::Error(reason) => {
                    outcome = Err(AdapterError::Transfer {
                        protocol: Protocol::Mta,
                        reason,
                    });
                    break;
                }
            }
        }

        let _ = handle.await;
        outcome
    }

    async fn accept(&self, _pending: ReceivePending, _destination: PathBuf) -> AdapterResult<()> {
        // The MTA engine accepts a transfer as part of its receive workflow,
        // so an accept decided here only confirms the request the engine
        // already made.
        Ok(())
    }

    async fn reject(&self, _pending: ReceivePending) -> AdapterResult<()> {
        // The MTA engine declines a transfer by not accepting the request, so
        // there is nothing to undo here.
        Ok(())
    }
}

/// Builds the pending transfer of an incoming MTA request.
fn pending_from_request(request: &ReceiveRequest) -> ReceivePending {
    let mut peer = Peer::new(
        Protocol::Mta,
        request.sender_name.clone(),
        request.sender_name.clone(),
    );
    peer.device_type = Some(DeviceType::Mobile);

    ReceivePending {
        protocol: Protocol::Mta,
        transfer_id: format!("{}:{}", request.sender_name, request.file_name),
        sender: peer,
        files: vec![FileEntry {
            name: request.file_name.clone(),
            size: request.total_size,
            mime: None,
            sha256: None,
        }],
        total_size: request.total_size,
    }
}
