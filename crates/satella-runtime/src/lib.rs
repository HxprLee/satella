//! The Satella runtime.
//!
//! It starts every registered adapter concurrently, merges their discovery
//! events into one stream, and routes a send to the adapter that owns the
//! target peer. Adapters never see each other.

use std::sync::Arc;

use futures_util::stream::Stream;
use satella_core::adapter::{AdapterError, AdapterEvent, ProtocolAdapter};
use satella_core::peer::{Peer, ReceivePending, SendRequest};
use satella_core::{Error, Protocol, Result};

/// How many adapter events are buffered before the slowest consumer applies back pressure.
const EVENT_BUFFER: usize = 256;

/// The runtime holding all running adapters.
pub struct Runtime {
    adapters: Vec<Arc<dyn ProtocolAdapter>>,
    /// The sender every adapter's events are forwarded through.
    events_tx: tokio::sync::mpsc::Sender<AdapterEvent>,
    events: tokio::sync::mpsc::Receiver<AdapterEvent>,
    started: bool,
}

impl Runtime {
    /// Builds a runtime over the given adapters.
    ///
    /// The adapters are not started yet; [`Runtime::start`] does that.
    pub fn new(adapters: Vec<Arc<dyn ProtocolAdapter>>) -> Self {
        let (events_tx, events) = tokio::sync::mpsc::channel(EVENT_BUFFER);
        Self {
            adapters,
            events_tx,
            events,
            started: false,
        }
    }

    /// The protocols that are registered, in registration order.
    pub fn protocols(&self) -> Vec<Protocol> {
        self.adapters.iter().map(|a| a.protocol()).collect()
    }

    /// Starts every adapter concurrently.
    ///
    /// One failing adapter does not stop the others: it is reported and the
    /// remaining adapters keep running, because a missing protocol must not
    /// take the whole app down.
    pub async fn start(&mut self) -> Vec<AdapterError> {
        let mut failures = Vec::new();

        for adapter in &self.adapters {
            let (tx, rx) = tokio::sync::mpsc::channel(EVENT_BUFFER);
            if let Err(err) = adapter.start(tx).await {
                failures.push(err);
                continue;
            }

            let mut rx = rx;
            let sender = self.events_tx.clone();

            // Each adapter gets its own task, so one slow protocol cannot
            // delay the events of another.
            tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    if sender.send(event).await.is_err() {
                        break;
                    }
                }
            });
        }

        // Announcing is what makes this device visible to the peers of the
        // protocols that are discovered by announcement. It runs in the
        // background, because an announcement burst takes a few seconds and
        // must not delay the start.
        for adapter in &self.adapters {
            let adapter = adapter.clone();
            tokio::spawn(async move {
                if let Err(err) = adapter.announce().await {
                    tracing::warn!(
                        protocol = %adapter.protocol(),
                        "could not announce this device: {err}"
                    );
                }
            });
        }

        self.started = true;
        failures
    }

    /// Stops every adapter.
    pub async fn stop(&mut self) {
        for adapter in &self.adapters {
            if let Err(err) = adapter.stop().await {
                tracing::warn!(
                    protocol = %adapter.protocol(),
                    "could not stop the adapter: {err}"
                );
            }
        }
        self.started = false;
    }

    /// The peers known across all adapters.
    pub fn peers(&self) -> Vec<Peer> {
        let mut peers: Vec<Peer> = self.adapters.iter().flat_map(|a| a.peers()).collect();
        peers.sort_by(|a, b| {
            a.protocol
                .cmp(&b.protocol)
                .then_with(|| a.alias.cmp(&b.alias))
        });
        peers
    }

    /// The peers known for one protocol.
    pub fn peers_for(&self, protocol: Protocol) -> Vec<Peer> {
        self.peers()
            .into_iter()
            .filter(|p| p.protocol == protocol)
            .collect()
    }

    /// The next event from any adapter.
    ///
    /// Returns `None` once every adapter's event channel is closed.
    pub async fn next_event(&mut self) -> Option<AdapterEvent> {
        self.events.recv().await
    }

    /// A stream of the merged events of all adapters.
    pub fn events(&mut self) -> impl Stream<Item = AdapterEvent> + '_ {
        futures_util::stream::unfold(self, |runtime| async move {
            runtime.next_event().await.map(|event| (event, runtime))
        })
    }

    /// Sends files to a peer, routing the request to the peer's own protocol.
    pub async fn send(&self, request: SendRequest) -> Result<()> {
        let adapter = self
            .adapters
            .iter()
            .find(|a| a.protocol() == request.peer.protocol)
            .ok_or(Error::AdapterNotRunning {
                protocol: request.peer.protocol,
            })?;
        adapter.send(request).await.map_err(Error::Adapter)
    }

    /// Accepts an incoming transfer, routing it to the sender's own protocol.
    pub async fn accept(
        &self,
        pending: ReceivePending,
        destination: std::path::PathBuf,
    ) -> Result<()> {
        let adapter = self
            .adapters
            .iter()
            .find(|a| a.protocol() == pending.protocol)
            .ok_or(Error::AdapterNotRunning {
                protocol: pending.protocol,
            })?;
        adapter
            .accept(pending, destination)
            .await
            .map_err(Error::Adapter)
    }

    /// Rejects an incoming transfer.
    pub async fn reject(&self, pending: ReceivePending) -> Result<()> {
        let adapter = self
            .adapters
            .iter()
            .find(|a| a.protocol() == pending.protocol)
            .ok_or(Error::AdapterNotRunning {
                protocol: pending.protocol,
            })?;
        adapter.reject(pending).await.map_err(Error::Adapter)
    }
}
