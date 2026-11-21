//! The trait every protocol adapter implements.
//!
//! An adapter owns exactly one protocol. The runtime starts all of them
//! concurrently, merges their discovery streams, and routes a send to the
//! adapter that owns the target peer. Adding a protocol means adding one more
//! adapter crate; nothing in the core or the runtime changes.

use async_trait::async_trait;
use std::sync::Arc;

use crate::peer::{Peer, ReceivePending, SendRequest, TransferProgress};
use crate::Protocol;

/// An event emitted by a running adapter.
#[derive(Clone, Debug)]
pub enum AdapterEvent {
    /// A peer was discovered.
    Discovered(Peer),
    /// A previously discovered peer changed.
    Updated(Peer),
    /// A peer is no longer reachable.
    Lost(Peer),
    /// An incoming transfer is waiting for a decision.
    ReceivePending(ReceivePending),
    /// The progress of an outgoing or incoming transfer changed.
    Progress(TransferProgress),
}

/// The sender half of an adapter's event stream.
pub type EventSender = tokio::sync::mpsc::Sender<AdapterEvent>;

/// An error raised by an adapter.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    /// The adapter could not be started.
    #[error("could not start the {protocol} adapter: {reason}")]
    Startup {
        /// The protocol of the failing adapter.
        protocol: Protocol,
        /// Why it failed.
        reason: String,
    },
    /// The peer could not be reached.
    #[error("could not reach the {protocol} peer: {reason}")]
    Unreachable {
        /// The protocol of the peer.
        protocol: Protocol,
        /// Why it failed.
        reason: String,
    },
    /// The transfer failed.
    #[error("the {protocol} transfer failed: {reason}")]
    Transfer {
        /// The protocol of the transfer.
        protocol: Protocol,
        /// Why it failed.
        reason: String,
    },
    /// The peer is not known to this adapter.
    #[error("no {protocol} peer is known for this request")]
    UnknownPeer {
        /// The protocol of the peer.
        protocol: Protocol,
    },
}

/// The result of an adapter operation.
pub type AdapterResult<T> = Result<T, AdapterError>;

/// A running adapter, owned by the runtime.
#[async_trait]
pub trait ProtocolAdapter: Send + Sync + 'static {
    /// The protocol this adapter speaks.
    fn protocol(&self) -> Protocol;

    /// Starts the adapter and begins discovering peers.
    ///
    /// The adapter sends its events into `events`. It must not block on a full
    /// event channel: a slow consumer must never stall discovery.
    async fn start(&self, events: EventSender) -> AdapterResult<()>;

    /// Stops the adapter and releases its resources.
    async fn stop(&self) -> AdapterResult<()>;

    /// Announces this device to the network, so that the peers of this
    /// protocol can find it.
    ///
    /// Protocols that answer announcements with a request of their own need
    /// this; protocols that are discovered passively do not, and leave the
    /// default alone.
    async fn announce(&self) -> AdapterResult<()> {
        Ok(())
    }

    /// The peers this adapter currently knows about.
    fn peers(&self) -> Vec<Peer>;

    /// Sends files to a peer.
    async fn send(&self, request: SendRequest) -> AdapterResult<()>;

    /// Accepts an incoming transfer, writing the files into `destination`.
    async fn accept(
        &self,
        pending: ReceivePending,
        destination: std::path::PathBuf,
    ) -> AdapterResult<()>;

    /// Rejects an incoming transfer.
    async fn reject(&self, pending: ReceivePending) -> AdapterResult<()>;
}

/// A handle to a running adapter.
pub type SharedAdapter = Arc<dyn ProtocolAdapter>;
