//! The Google Quick Share (Nearby Share) adapter.
//!
//! The protocol engine is vendored from [Martichou/rquickshare] (GPL-3.0);
//! this crate only translates between that engine and Satella's
//! protocol-neutral types. See `THIRD_PARTY_NOTICES.md`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use satella_core::adapter::{
    AdapterError, AdapterEvent, AdapterResult, EventSender, ProtocolAdapter,
};
use satella_core::peer::{DeviceType, FileEntry, Peer, ReceivePending, SendFile, SendRequest};
use satella_core::Protocol;
use tokio::sync::{broadcast, mpsc};

use satella_quickshare_vendor::channel::{
    ChannelAction, ChannelDirection, ChannelMessage, TransferType,
};
use satella_quickshare_vendor::{
    DeviceType as VendorDeviceType, EndpointInfo, OutboundPayload, Visibility, RQS,
};

/// The configuration of the Quick Share adapter.
#[derive(Default)]
pub struct Config {
    /// The TCP port the Quick Share server listens on. `None` picks a free port.
    pub port: Option<u16>,
    /// The directory received files are written into.
    pub download_path: Option<PathBuf>,
}

/// The Quick Share adapter.
pub struct QuickShareAdapter {
    config: Config,
    state: tokio::sync::Mutex<Option<InnerState>>,
}

struct InnerState {
    engine: RQS,
    /// The channel the engine sends its send requests on.
    send_tx: mpsc::Sender<satella_quickshare_vendor::SendInfo>,
    /// The peers seen on the network, by endpoint ID.
    peers: Arc<std::sync::Mutex<HashMap<String, Peer>>>,
}

impl QuickShareAdapter {
    /// Builds an adapter for the given configuration.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            state: tokio::sync::Mutex::new(None),
        }
    }
}

/// Translates the vendor's device type into Satella's.
fn map_device_type(device_type: Option<&VendorDeviceType>) -> Option<DeviceType> {
    match device_type {
        Some(VendorDeviceType::Phone) => Some(DeviceType::Mobile),
        Some(VendorDeviceType::Tablet) => Some(DeviceType::Mobile),
        Some(VendorDeviceType::Laptop) => Some(DeviceType::Desktop),
        _ => None,
    }
}

/// Builds the peer of a discovered Quick Share endpoint.
fn peer_from_endpoint(endpoint: &EndpointInfo) -> Peer {
    let alias = endpoint.name.clone().unwrap_or_else(|| endpoint.id.clone());
    let mut peer = Peer::new(Protocol::QuickShare, alias, endpoint.id.clone());
    peer.device_type = map_device_type(endpoint.rtype.as_ref());

    if let Some(ip) = endpoint.ip.as_ref().and_then(|ip| ip.parse().ok()) {
        peer.ip = Some(ip);
        let port = endpoint
            .port
            .as_ref()
            .and_then(|port| port.parse().ok())
            .unwrap_or(0);
        peer.address = Some(std::net::SocketAddr::new(ip, port));
    }
    peer
}

#[async_trait]
impl ProtocolAdapter for QuickShareAdapter {
    fn protocol(&self) -> Protocol {
        Protocol::QuickShare
    }

    async fn start(&self, events: EventSender) -> AdapterResult<()> {
        let mut engine = RQS::new(
            Visibility::Visible,
            self.config.port.map(u32::from),
            self.config.download_path.clone(),
        );

        let (send_tx, _send_rx) = engine.run().await.map_err(|err| AdapterError::Startup {
            protocol: Protocol::QuickShare,
            reason: err.to_string(),
        })?;

        let peers = Arc::new(std::sync::Mutex::new(HashMap::new()));

        // The endpoint stream of the discovery is forwarded to the runtime.
        {
            let (endpoint_tx, endpoint_rx) = broadcast::channel::<EndpointInfo>(64);
            engine
                .discovery(endpoint_tx.clone())
                .map_err(|err| AdapterError::Startup {
                    protocol: Protocol::QuickShare,
                    reason: err.to_string(),
                })?;

            let events = events.clone();
            let peers = peers.clone();
            tokio::spawn(async move {
                let mut stream = endpoint_rx;
                loop {
                    let endpoint = match stream.recv().await {
                        Ok(endpoint) => endpoint,
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    };

                    if endpoint.present == Some(false) {
                        peers.lock().unwrap().remove(&endpoint.id);
                        continue;
                    }

                    let peer = peer_from_endpoint(&endpoint);
                    peers
                        .lock()
                        .unwrap()
                        .insert(peer.fingerprint.clone(), peer.clone());

                    if events.send(AdapterEvent::Updated(peer)).await.is_err() {
                        break;
                    }
                }
            });
        }

        // The channel of the engine carries the state of every transfer, both
        // the ones this device sends and the ones it receives.
        {
            let events = events.clone();
            let peers = peers.clone();
            let mut message_rx = engine.message_sender.subscribe();
            tokio::spawn(async move {
                loop {
                    let message = match message_rx.recv().await {
                        Ok(message) => message,
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    };

                    if message.direction != ChannelDirection::LibToFront {
                        continue;
                    }

                    let Some(state) = message.state.as_ref() else {
                        continue;
                    };

                    // A transfer that waits for the user's consent is an
                    // incoming transfer.
                    if *state == satella_quickshare_vendor::State::WaitingForUserConsent {
                        if let Some(pending) = pending_from_message(&message) {
                            if events
                                .send(AdapterEvent::ReceivePending(pending))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }

                    // A transfer that was waiting for the consent and then
                    // ended on its own is gone, so the decision that was asked
                    // for can be withdrawn.
                    if message.rtype == Some(TransferType::Inbound)
                        && matches!(
                            state,
                            satella_quickshare_vendor::State::Cancelled
                                | satella_quickshare_vendor::State::Disconnected
                        )
                    {
                        let _ = events
                            .send(AdapterEvent::ReceiveExpired {
                                protocol: Protocol::QuickShare,
                                transfer_id: message.id.clone(),
                            })
                            .await;
                    }

                    if message.rtype == Some(TransferType::Inbound) {
                        let total = message
                            .meta
                            .as_ref()
                            .map(|m| m.total_bytes)
                            .unwrap_or_default();
                        let transferred = message
                            .meta
                            .as_ref()
                            .map(|m| m.ack_bytes)
                            .unwrap_or_default();
                        let _ = peers;
                        let _ = events
                            .send(AdapterEvent::Progress(
                                satella_core::peer::TransferProgress::Bytes { transferred, total },
                            ))
                            .await;
                    }
                }
            });
        }

        *self.state.lock().await = Some(InnerState {
            engine,
            send_tx,
            peers,
        });

        Ok(())
    }

    async fn stop(&self) -> AdapterResult<()> {
        if let Some(mut state) = self.state.lock().await.take() {
            state.engine.stop().await;
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
                protocol: Protocol::QuickShare,
                reason: "the adapter is not running".to_owned(),
            });
        };

        let peer = &request.peer;
        let Some(address) = peer.address else {
            return Err(AdapterError::UnknownPeer {
                protocol: Protocol::QuickShare,
            });
        };

        // The engine transfers files from the local filesystem, so a file
        // supplied as a stream cannot be sent over Quick Share.
        let mut files = Vec::new();
        for file in &request.files {
            let SendFile::Path(path) = file else {
                return Err(AdapterError::Transfer {
                    protocol: Protocol::QuickShare,
                    reason: "Quick Share can only send files from disk".to_owned(),
                });
            };
            files.push(path.to_string_lossy().to_string());
        }

        state
            .send_tx
            .send(satella_quickshare_vendor::SendInfo {
                id: peer.fingerprint.clone(),
                name: peer.alias.clone(),
                addr: address.to_string(),
                ob: OutboundPayload::Files(files),
            })
            .await
            .map_err(|err| AdapterError::Transfer {
                protocol: Protocol::QuickShare,
                reason: err.to_string(),
            })?;

        Ok(())
    }

    async fn accept(&self, pending: ReceivePending, _destination: PathBuf) -> AdapterResult<()> {
        // The engine writes the files into the download path it was started
        // with, so the destination of the runtime's accept is not used.
        let state = self.state.lock().await;
        let Some(state) = state.as_ref() else {
            return Err(AdapterError::Startup {
                protocol: Protocol::QuickShare,
                reason: "the adapter is not running".to_owned(),
            });
        };

        state
            .engine
            .message_sender
            .send(ChannelMessage {
                id: pending.transfer_id.clone(),
                direction: ChannelDirection::FrontToLib,
                action: Some(ChannelAction::AcceptTransfer),
                rtype: Some(TransferType::Inbound),
                state: None,
                meta: None,
            })
            .map_err(|err| AdapterError::Transfer {
                protocol: Protocol::QuickShare,
                reason: err.to_string(),
            })?;

        Ok(())
    }

    async fn reject(&self, pending: ReceivePending) -> AdapterResult<()> {
        let state = self.state.lock().await;
        let Some(state) = state.as_ref() else {
            return Err(AdapterError::Startup {
                protocol: Protocol::QuickShare,
                reason: "the adapter is not running".to_owned(),
            });
        };

        state
            .engine
            .message_sender
            .send(ChannelMessage {
                id: pending.transfer_id.clone(),
                direction: ChannelDirection::FrontToLib,
                action: Some(ChannelAction::RejectTransfer),
                rtype: Some(TransferType::Inbound),
                state: None,
                meta: None,
            })
            .map_err(|err| AdapterError::Transfer {
                protocol: Protocol::QuickShare,
                reason: err.to_string(),
            })?;

        Ok(())
    }
}

/// Builds the pending transfer of an incoming Quick Share request.
fn pending_from_message(message: &ChannelMessage) -> Option<ReceivePending> {
    let meta = message.meta.as_ref()?;
    let source = meta.source.as_ref()?;

    let mut peer = Peer::new(
        Protocol::QuickShare,
        source.name.clone(),
        message.id.clone(),
    );
    peer.device_type = map_device_type(Some(&source.device_type));

    let files: Vec<FileEntry> = meta
        .files
        .as_ref()
        .map(|files| {
            files
                .iter()
                .map(|name| FileEntry {
                    name: name.clone(),
                    size: 0,
                    mime: None,
                    sha256: None,
                })
                .collect()
        })
        .unwrap_or_default();

    Some(ReceivePending {
        protocol: Protocol::QuickShare,
        transfer_id: message.id.clone(),
        sender: peer,
        total_size: meta.total_bytes,
        files,
    })
}
