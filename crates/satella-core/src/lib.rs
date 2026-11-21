//! The protocol-neutral core of Satella.
//!
//! This crate holds the types every frontend sees and the trait every protocol
//! adapter implements. It contains no protocol logic and no I/O, so a new
//! protocol can be added without touching it.

pub mod adapter;
pub mod peer;
pub mod protocol;

pub use adapter::{
    AdapterError, AdapterEvent, AdapterResult, EventSender, ProtocolAdapter, SharedAdapter,
};
pub use peer::{
    DeviceType, FileEntry, Peer, PeerKey, ReceiveAccept, ReceivePending, SendFile, SendRequest,
    TransferProgress,
};
pub use protocol::Protocol;

/// The error type of the unified Satella API.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An adapter failed.
    #[error(transparent)]
    Adapter(#[from] AdapterError),
    /// The requested peer is not known to any running adapter.
    #[error("no peer matches this request")]
    UnknownPeer,
    /// The peer is known, but no adapter for its protocol is running.
    #[error("the {protocol} adapter is not running")]
    AdapterNotRunning {
        /// The protocol of the peer.
        protocol: Protocol,
    },
    /// The operation was cancelled.
    #[error("the operation was cancelled")]
    Cancelled,
    /// The operation failed for a reason that does not fit the other variants.
    #[error("{0}")]
    Other(String),
}

/// The result of a unified Satella API operation.
pub type Result<T> = std::result::Result<T, Error>;
