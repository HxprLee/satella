//! Protocol-neutral peer and transfer types.

use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use crate::Protocol;

/// The kind of device a peer is, as far as the protocol reports it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DeviceType {
    Mobile,
    Desktop,
    Web,
    Headless,
    Server,
}

/// A device discovered on the network, together with the protocol it speaks.
///
/// The protocol is part of the peer, not a property of the discovery channel:
/// the same physical device may be reachable over more than one protocol, and
/// a frontend must be able to tell them apart before offering to send.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Peer {
    /// The protocol this peer was discovered on.
    pub protocol: Protocol,
    /// The display name of the peer.
    pub alias: String,
    /// The fingerprint identifying the peer, used to deduplicate devices.
    pub fingerprint: String,
    /// The device model, when the protocol reports one.
    pub device_model: Option<String>,
    /// The device type category, when the protocol reports one.
    pub device_type: Option<DeviceType>,
    /// The protocol version implemented by the peer.
    pub version: Option<String>,
    /// The address the peer was seen at, when the protocol reveals one.
    pub address: Option<SocketAddr>,
    /// The raw IP address of the peer, when the protocol reveals one.
    pub ip: Option<IpAddr>,
}

impl Peer {
    /// Builds a peer with the fields every protocol provides.
    pub fn new(
        protocol: Protocol,
        alias: impl Into<String>,
        fingerprint: impl Into<String>,
    ) -> Self {
        Self {
            protocol,
            alias: alias.into(),
            fingerprint: fingerprint.into(),
            device_model: None,
            device_type: None,
            version: None,
            address: None,
            ip: None,
        }
    }

    /// The key that identifies this peer within one protocol.
    ///
    /// Peers are deduplicated per protocol: the same device seen over two
    /// protocols stays two entries, because each is reached differently.
    pub fn key(&self) -> PeerKey {
        PeerKey {
            protocol: self.protocol,
            fingerprint: self.fingerprint.clone(),
        }
    }
}

/// The identity of a peer within a single protocol.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PeerKey {
    /// The protocol the peer belongs to.
    pub protocol: Protocol,
    /// The peer's fingerprint.
    pub fingerprint: String,
}

/// A file offered for sending or receiving.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FileEntry {
    /// The file name as it should appear on the receiving side.
    pub name: String,
    /// The size of the file in bytes.
    pub size: u64,
    /// The file type, when the protocol reports one.
    pub mime: Option<String>,
    /// A SHA-256 checksum of the file, when the protocol uses one.
    pub sha256: Option<String>,
}

/// A request to send files to a peer.
///
/// The request is consumed by the send, because a file may be supplied as a
/// stream that cannot be cloned.
#[derive(Debug)]
pub struct SendRequest {
    /// The peer to send to.
    pub peer: Peer,
    /// The files to send.
    pub files: Vec<SendFile>,
}

/// A file to send.
///
/// The stream variant cannot be cloned, so a request holding one is consumed
/// by the send.
#[derive(Debug)]
pub enum SendFile {
    /// A file read from the local filesystem.
    Path(PathBuf),
    /// File content supplied by the application as a stream of chunks.
    Stream(tokio::sync::mpsc::Receiver<bytes::Bytes>),
}

/// A request to accept an incoming transfer.
#[derive(Clone, Debug)]
pub struct ReceiveAccept {
    /// The pending transfer being accepted.
    pub pending: ReceivePending,
    /// The directory the files are written into.
    pub destination: PathBuf,
}

/// An incoming transfer waiting for the user to accept or reject it.
///
/// The sender's protocol travels with the pending transfer so a frontend can
/// show which protocol the request arrived on before the user decides.
#[derive(Clone, Debug)]
pub struct ReceivePending {
    /// The protocol the sender used.
    pub protocol: Protocol,
    /// The identifier of this transfer within its protocol.
    ///
    /// The runtime hands it back when the transfer is accepted or rejected, so
    /// an adapter can match the decision to the session it opened.
    pub transfer_id: String,
    /// The sender, as it was discovered.
    pub sender: Peer,
    /// The files the sender offers.
    pub files: Vec<FileEntry>,
    /// The total size of all offered files in bytes.
    pub total_size: u64,
}

/// The progress of a transfer.
#[derive(Clone, Debug, PartialEq)]
pub enum TransferProgress {
    /// The transfer is being prepared.
    Starting,
    /// Bytes have been transferred.
    Bytes {
        /// The number of bytes transferred so far.
        transferred: u64,
        /// The total number of bytes to transfer.
        total: u64,
    },
    /// The transfer finished.
    Finished,
    /// The transfer failed.
    Failed {
        /// The reason the transfer failed.
        reason: String,
    },
}

impl TransferProgress {
    /// The fraction of the transfer that is done, between 0.0 and 1.0.
    pub fn fraction(&self) -> f64 {
        match self {
            TransferProgress::Starting => 0.0,
            TransferProgress::Bytes { transferred, total } => {
                if *total == 0 {
                    1.0
                } else {
                    *transferred as f64 / *total as f64
                }
            }
            TransferProgress::Finished => 1.0,
            TransferProgress::Failed { .. } => 0.0,
        }
    }
}
