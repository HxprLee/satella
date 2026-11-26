//! The Mutual Transfer Alliance adapter.
//!
//! The protocol engine is vendored from [Tinnci/cattysend] (MIT); this crate
//! only translates between that engine and Satella's protocol-neutral types.
//! See `THIRD_PARTY_NOTICES.md`.
//!
//! The MTA protocol discovers peers over Bluetooth Low Energy and transfers
//! the files over a Wi-Fi Direct group, so the adapter needs a working
//! Bluetooth adapter and a Wi-Fi interface. Receiving goes through BlueZ and
//! NetworkManager; sending creates the Wi-Fi Direct group owner through
//! wpa_supplicant. That API is root-only, so an ordinary-user process calls
//! the narrow `satella-mta-helper` instead of elevating the whole CLI. Running
//! as root (or the hardware probes) uses the in-process path.

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
use tokio::sync::oneshot;

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
    /// Where the persistent identity is kept.
    ///
    /// The identity is the ECDH key pair and the sender ID. It has to
    /// survive a restart, otherwise the peers see this device as a new one
    /// every time it starts.
    pub identity_path: PathBuf,
    /// Whether an incoming transfer is taken without asking.
    ///
    /// The MTA sender waits for the answer, so this decides whether the
    /// vendor is asked at all. When it is false, the vendor waits for the
    /// application to accept or reject the transfer.
    pub auto_accept: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_name: "Satella Device".to_owned(),
            wifi_interface: "wlan0".to_owned(),
            output_dir: dirs::download_dir().unwrap_or_else(|| PathBuf::from(".")),
            identity_path: default_identity_path(),
            auto_accept: true,
        }
    }
}

/// The default place the persistent identity is kept.
fn default_identity_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("satella")
        .join("mta-identity")
}

/// The MTA adapter.
pub struct MtaAdapter {
    config: Config,
    state: tokio::sync::Mutex<Option<InnerState>>,
}

struct InnerState {
    /// The peers seen in the last scan, by their BLE address.
    peers: Arc<std::sync::Mutex<HashMap<String, Peer>>>,
    /// The vendor's questions, waiting for the application's answer.
    ///
    /// The key is the transfer id, so an answer reaches the right transfer.
    answers: Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<bool>>>>,
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
        let answers = Arc::new(std::sync::Mutex::new(HashMap::new()));

        // The receiver runs in the background: it advertises this device over
        // BLE and waits for a sender to offer files.
        {
            let events = events.clone();
            let config = self.config.clone();
            let answers = answers.clone();
            tokio::spawn(async move {
                loop {
                    // The vendor's options type is not cloneable, so it is
                    // rebuilt for every attempt.
                    let options = satella_mta_vendor::workflow::ReceiveOptions {
                        device_name: config.device_name.clone(),
                        wifi_interface: config.wifi_interface.clone(),
                        output_dir: config.output_dir.clone(),
                        auto_accept: config.auto_accept,
                        brand_id: satella_mta_vendor::config::BrandId::Unknown,
                        supports_5ghz: true,
                        identity_path: Some(config.identity_path.clone()),
                    };

                    let Ok(receiver) = satella_mta_vendor::workflow::Receiver::new(options) else {
                        tracing::error!("could not start the MTA receiver");
                        return;
                    };

                    let (callback, mut event_rx) =
                        satella_mta_vendor::workflow::SimpleReceiveCallback::new(
                            config.auto_accept,
                        );

                    let handle = tokio::spawn(async move { receiver.start(&callback).await });

                    while let Some(event) = event_rx.recv().await {
                        match event {
                            ReceiveEvent::Status(status) => {
                                tracing::debug!(status);
                            }
                            ReceiveEvent::Request { request, answer } => {
                                let receive_pending = pending_from_request(&request);

                                // The answer is kept until the application
                                // accepts or rejects the transfer. The vendor
                                // waits for it, so the sender waits too.
                                answers
                                    .lock()
                                    .unwrap()
                                    .insert(receive_pending.transfer_id.clone(), answer);

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

                    // The receiver is gone, so the transfers it was waiting
                    // for are gone too. They are refused, and the application
                    // is told, so it can take the question back.
                    let unanswered: Vec<String> = {
                        let mut answers = answers.lock().unwrap();
                        let ids = answers.keys().cloned().collect();
                        answers.clear();
                        ids
                    };
                    for id in unanswered {
                        let _ = events
                            .send(AdapterEvent::ReceiveExpired {
                                protocol: Protocol::Mta,
                                transfer_id: id,
                            })
                            .await;
                    }
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

        *self.state.lock().await = Some(InnerState { peers, answers });

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
            identity_path: Some(self.config.identity_path.clone()),
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

        // The vendor returns its failures as the task's result, not through
        // the callback, so the task's outcome is what decides success.
        // Without this a failed send is reported as a successful one.
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                outcome = Err(AdapterError::Transfer {
                    protocol: Protocol::Mta,
                    reason: err.to_string(),
                });
            }
            Err(err) => {
                outcome = Err(AdapterError::Transfer {
                    protocol: Protocol::Mta,
                    reason: err.to_string(),
                });
            }
        }
        outcome
    }

    async fn accept(&self, pending: ReceivePending, _destination: PathBuf) -> AdapterResult<()> {
        // The vendor waits for this answer, so accepting is what tells the
        // sender to go ahead with the files.
        self.answer(&pending, true).await
    }

    async fn reject(&self, pending: ReceivePending) -> AdapterResult<()> {
        // The vendor waits for this answer, so refusing is what tells the
        // sender that the transfer is not wanted.
        self.answer(&pending, false).await
    }
}

impl MtaAdapter {
    /// Answers the vendor's question about an incoming transfer.
    ///
    /// The answer is what the vendor's transfer task is waiting for, so the
    /// sender is answered through it.
    async fn answer(&self, pending: &ReceivePending, accept: bool) -> AdapterResult<()> {
        let state = self.state.lock().await;
        let Some(state) = state.as_ref() else {
            return Err(AdapterError::Startup {
                protocol: Protocol::Mta,
                reason: "the adapter is not running".to_owned(),
            });
        };

        let Some(answer) = state.answers.lock().unwrap().remove(&pending.transfer_id) else {
            return Err(AdapterError::Transfer {
                protocol: Protocol::Mta,
                reason: "the transfer is no longer waiting for a decision".to_owned(),
            });
        };

        // A dropped answer means the vendor gave up on its own.
        let _ = answer.send(accept);
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
        // The task id is unique per transfer, so the answer reaches the right
        // transfer when two of them arrive at once.
        transfer_id: if request.task_id.is_empty() {
            format!("{}:{}", request.sender_name, request.file_name)
        } else {
            request.task_id.clone()
        },
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
