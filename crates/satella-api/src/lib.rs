//! The public surface of Satella.
//!
//! A frontend builds an [`Engine`], starts it, and then listens to its
//! events. The engine runs every protocol adapter at the same time, so one
//! device list covers LocalSend, Quick Share, and the Mutual Transfer
//! Alliance.
//!
//! Every peer and every pending transfer carries the protocol it came from,
//! so a frontend can label them without knowing the protocols themselves.
//!
//! The types here are the ones the UDL file describes; the UniFFI scaffolding
//! is included at the end of the file.

// The UniFFI scaffolding is generated code. It has to live at the crate
// root, because it refers to `crate::UniFfiTag` and to the types below, so
// the lints that fire on it are silenced crate-wide rather than on the
// macro call.
#![allow(clippy::large_const_arrays)]

use std::path::PathBuf;
use std::sync::Arc;

use satella_core::adapter::{AdapterEvent, ProtocolAdapter};
use satella_core::peer::{
    FileEntry as CoreFileEntry, Peer as CorePeer, ReceivePending as CoreReceivePending,
};
use satella_core::peer::{SendFile, SendRequest};
use satella_core::TransferProgress as CoreProgress;
use satella_runtime::Runtime;

/// The version of the Satella engine.
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

/// The protocol a peer speaks.
///
/// The protocol is part of the peer, not a property of the discovery
/// channel: the same physical device may be reachable over more than one
/// protocol, and a frontend must be able to tell them apart before offering
/// to send.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Protocol {
    /// The LocalSend protocol.
    LocalSend,
    /// The Google Quick Share protocol.
    QuickShare,
    /// The Mutual Transfer Alliance protocol.
    Mta,
}

impl From<Protocol> for satella_core::Protocol {
    fn from(protocol: Protocol) -> Self {
        match protocol {
            Protocol::LocalSend => satella_core::Protocol::LocalSend,
            Protocol::QuickShare => satella_core::Protocol::QuickShare,
            Protocol::Mta => satella_core::Protocol::Mta,
        }
    }
}

impl From<satella_core::Protocol> for Protocol {
    fn from(protocol: satella_core::Protocol) -> Self {
        match protocol {
            satella_core::Protocol::LocalSend => Protocol::LocalSend,
            satella_core::Protocol::QuickShare => Protocol::QuickShare,
            satella_core::Protocol::Mta => Protocol::Mta,
        }
    }
}

/// A device discovered on the network.
#[derive(Clone, Debug)]
pub struct Peer {
    /// The protocol the peer speaks.
    pub protocol: Protocol,
    /// The display name of the peer.
    pub alias: String,
    /// The fingerprint identifying the peer.
    pub fingerprint: String,
    /// The device model, when the protocol reports one.
    pub device_model: Option<String>,
    /// The device type, when the protocol reports one.
    pub device_type: Option<String>,
    /// The protocol version the peer implements.
    pub version: Option<String>,
    /// The address the peer was seen at.
    pub address: Option<String>,
}

impl From<CorePeer> for Peer {
    fn from(peer: CorePeer) -> Self {
        Self {
            protocol: peer.protocol.into(),
            alias: peer.alias,
            fingerprint: peer.fingerprint,
            device_model: peer.device_model,
            device_type: peer.device_type.map(|t| format!("{t:?}")),
            version: peer.version,
            address: peer.address.map(|a| a.to_string()),
        }
    }
}

impl Peer {
    /// The core peer this frontend peer stands for.
    fn into_core(self) -> CorePeer {
        CorePeer {
            protocol: self.protocol.into(),
            alias: self.alias,
            fingerprint: self.fingerprint,
            device_model: self.device_model,
            device_type: None,
            version: self.version,
            address: self
                .address
                .as_ref()
                .and_then(|a| a.parse::<std::net::SocketAddr>().ok()),
            ip: None,
        }
    }
}

/// A file offered by a sender.
#[derive(Clone, Debug)]
pub struct FileEntry {
    /// The file name as it should appear on the receiving side.
    pub name: String,
    /// The size of the file in bytes.
    pub size: u64,
    /// The file type, when the protocol reports one.
    pub mime: Option<String>,
}

impl From<CoreFileEntry> for FileEntry {
    fn from(file: CoreFileEntry) -> Self {
        Self {
            name: file.name,
            size: file.size,
            mime: file.mime,
        }
    }
}

/// An incoming transfer waiting for a decision.
///
/// The sender's protocol travels with the pending transfer, so a frontend can
/// badge the request before the user accepts or rejects it.
#[derive(Clone, Debug)]
pub struct ReceivePending {
    /// The protocol the sender used.
    pub protocol: Protocol,
    /// The identifier of the transfer within its protocol.
    pub transfer_id: String,
    /// The sender of the transfer.
    pub sender: Peer,
    /// The files the sender offers.
    pub files: Vec<FileEntry>,
    /// The total size of all offered files in bytes.
    pub total_size: u64,
}

impl From<CoreReceivePending> for ReceivePending {
    fn from(pending: CoreReceivePending) -> Self {
        Self {
            protocol: pending.protocol.into(),
            transfer_id: pending.transfer_id,
            sender: pending.sender.into(),
            files: pending.files.into_iter().map(FileEntry::from).collect(),
            total_size: pending.total_size,
        }
    }
}

impl ReceivePending {
    /// The core pending transfer this frontend pending stands for.
    fn into_core(self) -> CoreReceivePending {
        CoreReceivePending {
            protocol: self.protocol.into(),
            transfer_id: self.transfer_id,
            sender: self.sender.into_core(),
            files: self
                .files
                .into_iter()
                .map(|f| CoreFileEntry {
                    name: f.name,
                    size: f.size,
                    mime: f.mime,
                    sha256: None,
                })
                .collect(),
            total_size: self.total_size,
        }
    }
}

/// The progress of a transfer.
#[derive(Clone, Debug)]
pub struct TransferProgress {
    /// The number of bytes transferred so far.
    pub transferred: u64,
    /// The total number of bytes to transfer.
    pub total: u64,
    /// Whether the transfer finished.
    pub finished: bool,
    /// The reason the transfer failed, when it did.
    pub error: Option<String>,
}

impl From<CoreProgress> for TransferProgress {
    fn from(progress: CoreProgress) -> Self {
        match progress {
            CoreProgress::Starting => Self {
                transferred: 0,
                total: 0,
                finished: false,
                error: None,
            },
            CoreProgress::Bytes { transferred, total } => Self {
                transferred,
                total,
                finished: false,
                error: None,
            },
            CoreProgress::Finished => Self {
                transferred: 0,
                total: 0,
                finished: true,
                error: None,
            },
            CoreProgress::Failed { reason } => Self {
                transferred: 0,
                total: 0,
                finished: false,
                error: Some(reason),
            },
        }
    }
}

/// An event of the unified engine.
///
/// The kind is one of `discovered`, `updated`, `lost`, `receive_pending` or
/// `progress`; the other fields are filled in according to the kind.
#[derive(Clone, Debug)]
pub struct EngineEvent {
    /// The kind of the event.
    pub kind: String,
    /// The peer the event is about, for the discovery events.
    pub peer: Option<Peer>,
    /// The pending transfer, for the receive events.
    pub pending: Option<ReceivePending>,
    /// The progress, for the progress events.
    pub progress: Option<TransferProgress>,
}

/// An error of the Satella engine.
#[derive(Debug, thiserror::Error)]
pub enum SatellaError {
    /// The requested protocol has no running adapter.
    #[error("no adapter for protocol {message}")]
    UnknownProtocol {
        /// The protocol that was asked for.
        message: String,
    },
    /// The operation failed inside a protocol adapter.
    #[error("{message}")]
    Adapter {
        /// What the adapter reported.
        message: String,
    },
    /// The engine could not be started.
    #[error("could not start the engine: {message}")]
    Startup {
        /// What went wrong.
        message: String,
    },
}

/// The unified Satella engine.
///
/// The engine owns one runtime and every protocol adapter. All its methods
/// block until the operation finishes, because the UniFFI boundary is
/// synchronous.
pub struct Engine {
    runtime: tokio::sync::Mutex<Runtime>,
    /// The Tokio runtime the engine runs on.
    ///
    /// ponytail: one runtime for the whole engine. It is enough for the file
    /// transfers of a phone or a desktop; a platform that needs the engine to
    /// share a runtime with the rest of the app can pass its own in later.
    tokio: Arc<tokio::runtime::Runtime>,
}

impl Engine {
    /// Builds an engine with every protocol adapter.
    pub fn new() -> Self {
        let tokio = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("the Tokio runtime must build"),
        );

        let adapters: Vec<Arc<dyn ProtocolAdapter>> = vec![
            Arc::new(satella_localsend::LocalSendAdapter::new(
                satella_localsend::Config::default(),
            )),
            Arc::new(satella_quickshare::QuickShareAdapter::new(
                satella_quickshare::Config::default(),
            )),
            Arc::new(satella_mta::MtaAdapter::new(satella_mta::Config::default())),
        ];

        Self {
            runtime: tokio::sync::Mutex::new(Runtime::new(adapters)),
            tokio,
        }
    }

    /// Starts every adapter and returns the protocols that are running.
    pub fn start(&self) -> Result<Vec<String>, SatellaError> {
        let tokio = self.tokio.clone();
        let mut guard = self.runtime.blocking_lock();
        tokio.block_on(async {
            let failures = guard.start().await;
            for failure in &failures {
                tracing::warn!("{failure}");
            }
            Ok(guard
                .protocols()
                .into_iter()
                .map(|p| p.as_str().to_owned())
                .collect())
        })
    }

    /// The peers known across all adapters.
    pub fn peers(&self) -> Vec<Peer> {
        let guard = self.runtime.blocking_lock();
        guard.peers().into_iter().map(Peer::from).collect()
    }

    /// The peers known for one protocol.
    pub fn peers_for(&self, protocol: Protocol) -> Vec<Peer> {
        let guard = self.runtime.blocking_lock();
        guard
            .peers_for(protocol.into())
            .into_iter()
            .map(Peer::from)
            .collect()
    }

    /// The next event of any adapter. Blocks until one arrives.
    pub fn next_event(&self) -> Option<EngineEvent> {
        let tokio = self.tokio.clone();
        let mut guard = self.runtime.blocking_lock();
        tokio.block_on(async {
            let event = guard.next_event().await?;
            Some(match event {
                AdapterEvent::Discovered(peer) => EngineEvent {
                    kind: "discovered".to_owned(),
                    peer: Some(peer.into()),
                    pending: None,
                    progress: None,
                },
                AdapterEvent::Updated(peer) => EngineEvent {
                    kind: "updated".to_owned(),
                    peer: Some(peer.into()),
                    pending: None,
                    progress: None,
                },
                AdapterEvent::Lost(peer) => EngineEvent {
                    kind: "lost".to_owned(),
                    peer: Some(peer.into()),
                    pending: None,
                    progress: None,
                },
                AdapterEvent::ReceivePending(pending) => EngineEvent {
                    kind: "receive_pending".to_owned(),
                    peer: None,
                    pending: Some(pending.into()),
                    progress: None,
                },
                AdapterEvent::Progress(progress) => EngineEvent {
                    kind: "progress".to_owned(),
                    peer: None,
                    pending: None,
                    progress: Some(progress.into()),
                },
            })
        })
    }

    /// Sends files to a peer.
    pub fn send(&self, peer: Peer, file_paths: Vec<String>) -> Result<(), SatellaError> {
        let tokio = self.tokio.clone();
        let guard = self.runtime.blocking_lock();
        let files = file_paths
            .into_iter()
            .map(|path| SendFile::Path(PathBuf::from(path)))
            .collect();

        tokio.block_on(async {
            guard
                .send(SendRequest {
                    peer: peer.into_core(),
                    files,
                })
                .await
                .map_err(|err| SatellaError::Adapter {
                    message: err.to_string(),
                })
        })
    }

    /// Accepts an incoming transfer, writing the files into the directory.
    pub fn accept(&self, pending: ReceivePending, destination: String) -> Result<(), SatellaError> {
        let tokio = self.tokio.clone();
        let guard = self.runtime.blocking_lock();
        tokio.block_on(async {
            guard
                .accept(pending.into_core(), PathBuf::from(destination))
                .await
                .map_err(|err| SatellaError::Adapter {
                    message: err.to_string(),
                })
        })
    }

    /// Rejects an incoming transfer.
    pub fn reject(&self, pending: ReceivePending) -> Result<(), SatellaError> {
        let tokio = self.tokio.clone();
        let guard = self.runtime.blocking_lock();
        tokio.block_on(async {
            guard
                .reject(pending.into_core())
                .await
                .map_err(|err| SatellaError::Adapter {
                    message: err.to_string(),
                })
        })
    }

    /// Stops every adapter.
    pub fn stop(&self) {
        let tokio = self.tokio.clone();
        let mut guard = self.runtime.blocking_lock();
        tokio.block_on(async {
            guard.stop().await;
        });
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

uniffi::include_scaffolding!("satella");
