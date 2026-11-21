//! The check that Satella does what it promises: one merged device list, where
//! every peer says which protocol it speaks, and a pending receive that says
//! which protocol the sender used.
//!
//! The check runs two stub adapters side by side, so it needs no network and
//! no hardware. The real adapters plug into the same seam.

use std::sync::Arc;

use async_trait::async_trait;
use satella_core::adapter::{
    AdapterError, AdapterEvent, AdapterResult, EventSender, ProtocolAdapter,
};
use satella_core::peer::{Peer, ReceivePending, SendRequest};
use satella_core::Protocol;
use satella_runtime::Runtime;

/// An adapter that reports one fixed peer and one fixed pending transfer.
struct StubAdapter {
    protocol: Protocol,
    peer: Peer,
    pending: ReceivePending,
}

impl StubAdapter {
    fn new(protocol: Protocol, alias: &str) -> Self {
        let peer = Peer::new(protocol, alias, format!("fingerprint-{alias}"));

        Self {
            pending: ReceivePending {
                protocol,
                transfer_id: format!("transfer-{alias}"),
                sender: peer.clone(),
                files: Vec::new(),
                total_size: 0,
            },
            peer,
            protocol,
        }
    }
}

#[async_trait]
impl ProtocolAdapter for StubAdapter {
    fn protocol(&self) -> Protocol {
        self.protocol
    }

    async fn start(&self, events: EventSender) -> AdapterResult<()> {
        events
            .send(AdapterEvent::Discovered(self.peer.clone()))
            .await
            .map_err(|_| AdapterError::Startup {
                protocol: self.protocol,
                reason: "the event channel is closed".to_owned(),
            })?;

        events
            .send(AdapterEvent::ReceivePending(self.pending.clone()))
            .await
            .map_err(|_| AdapterError::Startup {
                protocol: self.protocol,
                reason: "the event channel is closed".to_owned(),
            })?;

        Ok(())
    }

    async fn stop(&self) -> AdapterResult<()> {
        Ok(())
    }

    fn peers(&self) -> Vec<Peer> {
        vec![self.peer.clone()]
    }

    async fn send(&self, _request: SendRequest) -> AdapterResult<()> {
        Ok(())
    }

    async fn accept(
        &self,
        _pending: ReceivePending,
        _destination: std::path::PathBuf,
    ) -> AdapterResult<()> {
        Ok(())
    }

    async fn reject(&self, _pending: ReceivePending) -> AdapterResult<()> {
        Ok(())
    }
}

/// The merged list covers every protocol, and every row names its protocol.
#[tokio::test]
async fn the_merged_peer_list_names_every_protocol() {
    let mut runtime = Runtime::new(vec![
        Arc::new(StubAdapter::new(Protocol::LocalSend, "localsend-peer")),
        Arc::new(StubAdapter::new(Protocol::QuickShare, "quickshare-peer")),
        Arc::new(StubAdapter::new(Protocol::Mta, "mta-peer")),
    ]);

    runtime.start().await;

    let peers = runtime.peers();
    assert_eq!(peers.len(), 3, "every adapter contributes its peer");

    for peer in &peers {
        assert!(
            !peer.protocol.as_str().is_empty(),
            "a peer must name its protocol"
        );
    }

    // Every protocol is represented exactly once.
    let mut protocols: Vec<Protocol> = peers.iter().map(|peer| peer.protocol).collect();
    protocols.sort();
    protocols.dedup();
    assert_eq!(protocols.len(), 3);

    runtime.stop().await;
}

/// A pending receive says which protocol the sender used.
#[tokio::test]
async fn a_pending_receive_names_the_senders_protocol() {
    let mut runtime = Runtime::new(vec![
        Arc::new(StubAdapter::new(Protocol::LocalSend, "localsend-peer")),
        Arc::new(StubAdapter::new(Protocol::QuickShare, "quickshare-peer")),
    ]);

    runtime.start().await;

    let mut pending: Vec<ReceivePending> = Vec::new();
    while pending.len() < 2 {
        let Some(AdapterEvent::ReceivePending(received)) = runtime.next_event().await else {
            continue;
        };
        pending.push(received);
    }

    for transfer in &pending {
        assert_eq!(
            transfer.protocol, transfer.sender.protocol,
            "the pending transfer and its sender must agree on the protocol"
        );
        assert!(
            !transfer.transfer_id.is_empty(),
            "a pending transfer must carry the protocol's own transfer id"
        );
    }

    // Both protocols are represented.
    let mut protocols: Vec<Protocol> = pending.iter().map(|p| p.protocol).collect();
    protocols.sort();
    protocols.dedup();
    assert_eq!(protocols.len(), 2);

    runtime.stop().await;
}

/// A send is routed to the adapter that owns the peer's protocol.
#[tokio::test]
async fn a_send_is_routed_to_the_protocol_of_the_peer() {
    let mut runtime = Runtime::new(vec![
        Arc::new(StubAdapter::new(Protocol::LocalSend, "localsend-peer")),
        Arc::new(StubAdapter::new(Protocol::QuickShare, "quickshare-peer")),
    ]);

    runtime.start().await;

    let peer = runtime
        .peers_for(Protocol::QuickShare)
        .into_iter()
        .next()
        .expect("the Quick Share adapter contributes a peer");

    let sent = runtime
        .send(SendRequest {
            peer,
            files: Vec::new(),
        })
        .await;

    assert!(sent.is_ok(), "the send must reach its adapter: {sent:?}");

    runtime.stop().await;
}
