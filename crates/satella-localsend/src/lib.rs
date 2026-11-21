//! The LocalSend protocol v2 adapter.
//!
//! The protocol engine is vendored from [localsend/localsend] (Apache-2.0);
//! this crate only translates between that engine and Satella's
//! protocol-neutral types. See `THIRD_PARTY_NOTICES.md`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use satella_core::adapter::{
    AdapterError, AdapterEvent, AdapterResult, EventSender, ProtocolAdapter,
};
use satella_core::peer::{DeviceType, FileEntry, Peer, ReceivePending, SendFile, SendRequest};
use satella_core::Protocol;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use satella_localsend_vendor::crypto::cert::{generate_self_signed, SelfSignedCert};
use satella_localsend_vendor::discovery::{
    DeviceChannel, DeviceIdentity, DiscoveredDevice, DiscoveryConfig, DiscoveryEvent,
    DiscoveryHandle, HttpChannel,
};
use satella_localsend_vendor::http::client::LsHttpClientV2;
use satella_localsend_vendor::http::dto_v2::PrepareUploadRequestDtoV2;
use satella_localsend_vendor::http::server::common::save::FileUploadTarget;
use satella_localsend_vendor::http::server::v2::{PrepareUploadDecisionV2, ServerEventV2};
use satella_localsend_vendor::http::server::web::WebConfig;
use satella_localsend_vendor::http::server::{ServerConfigV2, TlsConfig};
use satella_localsend_vendor::http::state::ClientInfo as ServerClientInfo;
use satella_localsend_vendor::model::discovery::{DeviceType as VendorDeviceType, ProtocolType};
use satella_localsend_vendor::model::transfer::FileContent;
use satella_localsend_vendor::multicast::{MulticastDevice, DEFAULT_MULTICAST_GROUP, DEFAULT_PORT};
use satella_localsend_vendor::util::interface::InterfaceFilter;

/// The LocalSend protocol version this adapter speaks.
const PROTOCOL_VERSION: &str = "2.2";

/// The configuration of the LocalSend adapter.
pub struct Config {
    /// The display name this device announces itself with.
    pub alias: String,
    /// The device model this device announces itself with.
    pub device_model: Option<String>,
    /// The device type this device announces itself with.
    pub device_type: Option<DeviceType>,
    /// The TCP port the LocalSend server listens on. `0` picks a free port.
    pub port: u16,
    /// Whether the server uses TLS.
    pub use_tls: bool,
    /// The network interfaces discovery is allowed to use.
    pub interface_filter: InterfaceFilter,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            alias: "Satella Device".to_owned(),
            device_model: None,
            device_type: None,
            // A free port, so that several Satella instances can run on one
            // machine. The announcement carries the port that was actually
            // bound, which is what the protocol asks for.
            port: 0,
            use_tls: true,
            interface_filter: InterfaceFilter::default(),
        }
    }
}

/// The LocalSend adapter.
pub struct LocalSendAdapter {
    config: Config,
    state: tokio::sync::Mutex<Option<InnerState>>,
}

struct InnerState {
    identity: SelfSignedCert,
    /// The port the HTTP server actually bound to.
    server_port: u16,
    stop_tx: oneshot::Sender<()>,
    /// The discovery handle, kept alive for as long as the adapter runs.
    ///
    /// Dropping it would stop the discovery.
    discovery: Arc<DiscoveryHandle>,
    /// Stops the discovery when the adapter stops.
    ///
    /// The vendor stops the discovery when this sender is dropped, so it must
    /// be kept alive for as long as the adapter runs.
    _discovery_stop_tx: oneshot::Sender<()>,
    peers: Arc<std::sync::Mutex<HashMap<String, Peer>>>,
    /// The incoming transfers waiting for a decision, by session ID.
    pending: Arc<std::sync::Mutex<HashMap<String, PendingUpload>>>,
    /// The accepted sessions, by session ID.
    sessions: Arc<std::sync::Mutex<HashMap<String, SessionState>>>,
}

/// An incoming transfer waiting for the application to decide.
struct PendingUpload {
    /// The vendor file IDs, keyed by file name.
    file_ids: HashMap<String, String>,
    decision_tx: oneshot::Sender<PrepareUploadDecisionV2>,
}

/// The state of an accepted upload session.
struct SessionState {
    /// Where the received files are written.
    destination: PathBuf,
}

impl LocalSendAdapter {
    /// Builds an adapter for the given configuration.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            state: tokio::sync::Mutex::new(None),
        }
    }

    /// The port the LocalSend server listens on, once the adapter is started.
    pub async fn server_port(&self) -> Option<u16> {
        self.state.lock().await.as_ref().map(|s| s.server_port)
    }
}

/// Translates the vendor's device type into Satella's.
fn map_device_type(device_type: Option<&VendorDeviceType>) -> Option<DeviceType> {
    match device_type {
        Some(VendorDeviceType::Mobile) => Some(DeviceType::Mobile),
        Some(VendorDeviceType::Desktop) => Some(DeviceType::Desktop),
        Some(VendorDeviceType::Web) => Some(DeviceType::Web),
        Some(VendorDeviceType::Headless) => Some(DeviceType::Headless),
        Some(VendorDeviceType::Server) => Some(DeviceType::Server),
        None => None,
    }
}

/// Builds the peer of a confirmed LocalSend device.
fn peer_from_device(device: &DiscoveredDevice) -> Peer {
    let mut peer = Peer::new(
        Protocol::LocalSend,
        device.alias.clone(),
        device.fingerprint.clone(),
    );
    peer.device_model = device.device_model.clone();
    peer.device_type = map_device_type(device.device_type.as_ref());
    peer.version = Some(device.version.clone());
    if let Some(channel) = device.channel.http() {
        peer.ip = channel.host.parse().ok();
        peer.address = Some(std::net::SocketAddr::new(
            peer.ip
                .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
            channel.port,
        ));
    }
    peer
}

#[async_trait]
impl ProtocolAdapter for LocalSendAdapter {
    fn protocol(&self) -> Protocol {
        Protocol::LocalSend
    }

    async fn start(&self, events: EventSender) -> AdapterResult<()> {
        let identity = generate_self_signed().map_err(|err| AdapterError::Startup {
            protocol: Protocol::LocalSend,
            reason: err.to_string(),
        })?;

        let (event_tx, mut event_rx) = mpsc::channel::<DiscoveryEvent>(256);
        let (server_event_tx, mut server_event_rx) = mpsc::channel::<ServerEventV2>(256);

        let server_device_type = client_info_device_type(&self.config);

        let server_client_info = ServerClientInfo {
            alias: self.config.alias.clone(),
            version: PROTOCOL_VERSION.to_owned(),
            device_model: self.config.device_model.clone(),
            device_type: server_device_type.clone(),
            token: identity.fingerprint.clone(),
        };

        let tls_config = if self.config.use_tls {
            Some(TlsConfig {
                cert: identity.certificate_pem.clone(),
                private_key: identity.private_key_pem.clone(),
            })
        } else {
            None
        };

        let (stop_tx, stop_rx) = oneshot::channel();
        let server = satella_localsend_vendor::http::server::start_with_port(
            self.config.port,
            tls_config,
            server_client_info,
            None,
            Some(ServerConfigV2 {
                pin: None,
                verify_checksums: true,
                event_tx: server_event_tx,
            }),
            WebConfig::default(),
            stop_rx,
        )
        .await
        .map_err(|err| AdapterError::Startup {
            protocol: Protocol::LocalSend,
            reason: err.to_string(),
        })?;

        let server_port = server.port();

        let multicast_device = MulticastDevice {
            alias: self.config.alias.clone(),
            version: PROTOCOL_VERSION.to_owned(),
            device_model: self.config.device_model.clone(),
            device_type: server_device_type.clone(),
            fingerprint: identity.fingerprint.clone(),
            port: server_port,
            protocol: if self.config.use_tls {
                ProtocolType::Https
            } else {
                ProtocolType::Http
            },
            download: false,
        };

        // The sender is kept in `InnerState`: dropping it stops the discovery,
        // so it must live as long as the adapter runs.
        let (discovery_stop_tx, discovery_stop_rx) = oneshot::channel();
        let discovery: DiscoveryHandle = satella_localsend_vendor::discovery::start(
            DiscoveryConfig {
                group: DEFAULT_MULTICAST_GROUP,
                group_v6: None,
                port: DEFAULT_PORT,
                interface_filter: self.config.interface_filter.clone(),
                device: multicast_device,
                identity: DeviceIdentity {
                    cert_pem: identity.certificate_pem.clone(),
                    private_key_pem: identity.private_key_pem.clone(),
                },
                timeout: satella_localsend_vendor::discovery::DEFAULT_DISCOVERY_TIMEOUT,
                event_tx: Some(event_tx),
            },
            discovery_stop_rx,
        )
        .await;

        let discovery = Arc::new(discovery);

        let peers = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let pending = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let sessions: Arc<std::sync::Mutex<HashMap<String, SessionState>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));

        // The discovery stream is forwarded to the runtime.
        {
            let events = events.clone();
            let peers = peers.clone();
            tokio::spawn(async move {
                while let Some(event) = event_rx.recv().await {
                    let (peer, lost) = match &event {
                        DiscoveryEvent::Discovered { device } => {
                            let peer = peer_from_device(device);
                            peers
                                .lock()
                                .unwrap()
                                .insert(peer.fingerprint.clone(), peer.clone());
                            (peer, false)
                        }
                        DiscoveryEvent::Updated { device } => {
                            let peer = peer_from_device(device);
                            peers
                                .lock()
                                .unwrap()
                                .insert(peer.fingerprint.clone(), peer.clone());
                            (peer, false)
                        }
                        DiscoveryEvent::Unreachable { device, .. } => {
                            let peer = peer_from_device(device);
                            peers.lock().unwrap().remove(&peer.fingerprint);
                            (peer, true)
                        }
                        DiscoveryEvent::MulticastFailed => continue,
                    };

                    let out = if lost {
                        AdapterEvent::Lost(peer)
                    } else {
                        AdapterEvent::Updated(peer)
                    };
                    if events.send(out).await.is_err() {
                        break;
                    }
                }
            });
        }

        // The server events drive the incoming transfers.
        {
            let events = events.clone();
            let pending = pending.clone();
            let sessions = sessions.clone();
            let discovery = discovery.clone();
            tokio::spawn(async move {
                while let Some(event) = server_event_rx.recv().await {
                    match event {
                        ServerEventV2::PrepareUpload {
                            session_id,
                            ip,
                            info,
                            files,
                            decision_tx,
                            ..
                        } => {
                            let mut peer = Peer::new(
                                Protocol::LocalSend,
                                info.alias.clone(),
                                info.fingerprint.clone(),
                            );
                            peer.device_model = info.device_model.clone();
                            peer.device_type = map_device_type(info.device_type.as_ref());
                            peer.version = Some(info.version.clone());
                            peer.ip = Some(ip.ip);

                            let entries: Vec<FileEntry> = files
                                .values()
                                .map(|file| FileEntry {
                                    name: file.file_name.clone(),
                                    size: file.size,
                                    mime: Some(file.file_type.clone()),
                                    sha256: file.sha256.clone(),
                                })
                                .collect();
                            let total_size = entries.iter().map(|f| f.size).sum();

                            let receive_pending = ReceivePending {
                                protocol: Protocol::LocalSend,
                                transfer_id: session_id.clone(),
                                sender: peer,
                                files: entries,
                                total_size,
                            };

                            // The file IDs are kept so the decision can name
                            // exactly the files the application accepted.
                            let file_ids: HashMap<String, String> = files
                                .iter()
                                .map(|(id, file)| (file.file_name.clone(), id.clone()))
                                .collect();

                            pending.lock().unwrap().insert(
                                session_id.clone(),
                                PendingUpload {
                                    file_ids,
                                    decision_tx,
                                },
                            );

                            if events
                                .send(AdapterEvent::ReceivePending(receive_pending))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        ServerEventV2::FileUpload {
                            session_id,
                            file_id,
                            file,
                            target_tx,
                        } => {
                            // The application decided where the content goes.
                            let destination = sessions
                                .lock()
                                .unwrap()
                                .get(&session_id)
                                .map(|session| session.destination.join(&file.file_name));

                            let Some(path) = destination else {
                                // The session is gone: the transfer was
                                // rejected, or the session ended early.
                                let _ = target_tx.send(FileUploadTarget::Path {
                                    path: PathBuf::from("/dev/null"),
                                    result_tx: oneshot::channel().0,
                                    progress_tx: None,
                                });
                                continue;
                            };

                            let (result_tx, result_rx) = oneshot::channel();
                            let (progress_tx, mut progress_rx) = mpsc::channel(64);

                            if target_tx
                                .send(FileUploadTarget::Path {
                                    path,
                                    result_tx,
                                    progress_tx: Some(progress_tx),
                                })
                                .is_err()
                            {
                                tracing::debug!("The upload session ended before the file arrived");
                                continue;
                            }

                            // The write result and the progress are reported
                            // to the runtime as they arrive.
                            let events = events.clone();
                            let sessions = sessions.clone();
                            let session_id = session_id.clone();
                            let file_id = file_id.clone();
                            tokio::spawn(async move {
                                let total = file.size;
                                while let Some(written) = progress_rx.recv().await {
                                    if events
                                        .send(AdapterEvent::Progress(
                                            satella_core::peer::TransferProgress::Bytes {
                                                transferred: written,
                                                total,
                                            },
                                        ))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                }

                                let outcome = result_rx.await.unwrap_or(Ok(()));
                                sessions.lock().unwrap().remove(&session_id);

                                let progress = match outcome {
                                    Ok(()) => satella_core::peer::TransferProgress::Finished,
                                    Err(reason) => {
                                        satella_core::peer::TransferProgress::Failed { reason }
                                    }
                                };
                                let _ = file_id;
                                let _ = events.send(AdapterEvent::Progress(progress)).await;
                            });
                        }
                        ServerEventV2::SessionEnd { session_id, .. } => {
                            sessions.lock().unwrap().remove(&session_id);
                        }
                        ServerEventV2::PrepareUploadAborted { session_id } => {
                            pending.lock().unwrap().remove(&session_id);
                            sessions.lock().unwrap().remove(&session_id);
                        }
                        ServerEventV2::Register { ip, info } => {
                            // A device registered with our HTTP server. That
                            // request is what confirms it, so the device goes
                            // into the discovery store and becomes a peer.
                            let device = DiscoveredDevice {
                                alias: info.alias.clone(),
                                version: info.version.clone(),
                                device_model: info.device_model.clone(),
                                device_type: info.device_type.clone(),
                                fingerprint: info.fingerprint.clone(),
                                channel: DeviceChannel::Http(HttpChannel {
                                    host: ip.to_string(),
                                    port: info.port,
                                    protocol: info.protocol,
                                }),
                                download: info.download,
                            };

                            if discovery.observe_inbound(device).await {
                                tracing::debug!("A device registered with us: {}", info.alias);
                            }
                        }
                        ServerEventV2::CancelReceived { .. }
                        | ServerEventV2::ListenerFailed { .. } => {}
                    }
                }
            });
        }

        *self.state.lock().await = Some(InnerState {
            identity,
            server_port,
            stop_tx,
            discovery,
            _discovery_stop_tx: discovery_stop_tx,
            peers,
            pending,
            sessions,
        });

        Ok(())
    }

    async fn stop(&self) -> AdapterResult<()> {
        if let Some(state) = self.state.lock().await.take() {
            let _ = state.stop_tx.send(());
        }
        Ok(())
    }

    async fn announce(&self) -> AdapterResult<()> {
        let state = self.state.lock().await;
        let Some(state) = state.as_ref() else {
            return Ok(());
        };

        state.discovery.announce().await;
        Ok(())
    }

    fn peers(&self) -> Vec<Peer> {
        // The peers are read without awaiting, so the adapter can be queried
        // from a synchronous context while the runtime is running.
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
                protocol: Protocol::LocalSend,
                reason: "the adapter is not running".to_owned(),
            });
        };

        let peer = &request.peer;
        let Some(address) = peer.address else {
            return Err(AdapterError::UnknownPeer {
                protocol: Protocol::LocalSend,
            });
        };
        let host = peer
            .ip
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| address.ip().to_string());

        let client = LsHttpClientV2::try_new(
            &state.identity.private_key_pem,
            &state.identity.certificate_pem,
            None,
            None,
        )
        .map_err(|err| AdapterError::Unreachable {
            protocol: Protocol::LocalSend,
            reason: err.to_string(),
        })?;

        let cancel = CancellationToken::new();
        let protocol = if self.config.use_tls {
            ProtocolType::Https
        } else {
            ProtocolType::Http
        };

        // The files are offered to the receiver first; it answers with the
        // files it accepted and the tokens to upload them with.
        let mut files = HashMap::new();
        for (index, file) in request.files.iter().enumerate() {
            let id = format!("file-{index}");
            let (name, size, mime) = match file {
                SendFile::Path(path) => {
                    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| id.clone());
                    let mime = mime_guess::from_path(path)
                        .first_raw()
                        .unwrap_or("application/octet-stream")
                        .to_owned();
                    (name, size, mime)
                }
                SendFile::Stream(_) => (
                    format!("{id}.bin"),
                    0,
                    "application/octet-stream".to_owned(),
                ),
            };
            files.insert(
                id.clone(),
                satella_localsend_vendor::model::transfer::FileDto {
                    id,
                    file_name: name,
                    size,
                    file_type: mime,
                    sha256: None,
                    preview: None,
                    metadata: None,
                },
            );
        }

        let prepare = PrepareUploadRequestDtoV2 {
            info: satella_localsend_vendor::http::dto_v2::RegisterDtoV2 {
                alias: self.config.alias.clone(),
                version: PROTOCOL_VERSION.to_owned(),
                device_model: self.config.device_model.clone(),
                device_type: client_info_device_type(&self.config),
                fingerprint: state.identity.fingerprint.clone(),
                port: state.server_port,
                protocol,
                download: false,
            },
            files,
        };

        let result = client
            .prepare_upload(
                protocol,
                &host,
                address.port(),
                None,
                prepare,
                None,
                cancel.clone(),
            )
            .await
            .map_err(|err| AdapterError::Unreachable {
                protocol: Protocol::LocalSend,
                reason: err.to_string(),
            })?;

        let Some(response) = result.response else {
            // The receiver accepted no file.
            return Ok(());
        };

        for (index, file) in request.files.into_iter().enumerate() {
            let file_id = format!("file-{index}");
            let Some(token) = response.files.get(&file_id) else {
                continue;
            };

            let body = match file {
                SendFile::Path(path) => FileContent::Path(path).into_stream().boxed(),
                SendFile::Stream(mut rx) => {
                    let stream = async_stream::stream! {
                        while let Some(chunk) = rx.recv().await {
                            yield Ok::<_, std::io::Error>(chunk);
                        }
                    };
                    stream.boxed()
                }
            };

            client
                .upload(
                    protocol,
                    &host,
                    address.port(),
                    None,
                    &response.session_id,
                    &file_id,
                    token,
                    reqwest::Body::wrap_stream(body),
                    cancel.clone(),
                )
                .await
                .map_err(|err| AdapterError::Transfer {
                    protocol: Protocol::LocalSend,
                    reason: err.to_string(),
                })?;
        }

        Ok(())
    }

    async fn accept(&self, pending: ReceivePending, destination: PathBuf) -> AdapterResult<()> {
        let state = self.state.lock().await;
        let Some(state) = state.as_ref() else {
            return Err(AdapterError::Startup {
                protocol: Protocol::LocalSend,
                reason: "the adapter is not running".to_owned(),
            });
        };

        let entry = state.pending.lock().unwrap().remove(&pending.transfer_id);
        let Some(entry) = entry else {
            return Err(AdapterError::UnknownPeer {
                protocol: Protocol::LocalSend,
            });
        };

        // Every offered file is accepted; the vendor answers with the files it
        // accepted, and only those are uploaded.
        let accepted: HashSet<String> = entry.file_ids.values().cloned().collect();

        entry
            .decision_tx
            .send(PrepareUploadDecisionV2::Accept(accepted))
            .map_err(|_| AdapterError::Transfer {
                protocol: Protocol::LocalSend,
                reason: "the receiver closed the session".to_owned(),
            })?;

        state
            .sessions
            .lock()
            .unwrap()
            .insert(pending.transfer_id.clone(), SessionState { destination });

        Ok(())
    }

    async fn reject(&self, pending: ReceivePending) -> AdapterResult<()> {
        let state = self.state.lock().await;
        let Some(state) = state.as_ref() else {
            return Err(AdapterError::Startup {
                protocol: Protocol::LocalSend,
                reason: "the adapter is not running".to_owned(),
            });
        };

        let entry = state.pending.lock().unwrap().remove(&pending.transfer_id);
        let Some(entry) = entry else {
            return Err(AdapterError::UnknownPeer {
                protocol: Protocol::LocalSend,
            });
        };

        entry
            .decision_tx
            .send(PrepareUploadDecisionV2::Decline)
            .map_err(|_| AdapterError::Transfer {
                protocol: Protocol::LocalSend,
                reason: "the receiver closed the session".to_owned(),
            })?;

        Ok(())
    }
}

/// The vendor device type of this device.
fn client_info_device_type(config: &Config) -> Option<VendorDeviceType> {
    config.device_type.map(|t| match t {
        DeviceType::Mobile => VendorDeviceType::Mobile,
        DeviceType::Desktop => VendorDeviceType::Desktop,
        DeviceType::Web => VendorDeviceType::Web,
        DeviceType::Headless => VendorDeviceType::Headless,
        DeviceType::Server => VendorDeviceType::Server,
    })
}
