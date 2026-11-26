//! The checks of what the command line reports about a send.
//!
//! A send that reports "sent N file(s)" when the peer refused the transfer
//! is a lie the user acts on. These checks drive the command's own send path
//! with a stub adapter, so the report is the one the command prints.

use std::sync::Arc;

use async_trait::async_trait;
use satella_core::adapter::{
    AdapterError, AdapterEvent, AdapterResult, EventSender, ProtocolAdapter,
};
use satella_core::peer::{Peer, ReceivePending, SendRequest};
use satella_core::Protocol;
use satella_runtime::Runtime;

/// An adapter whose send fails the way a refused transfer fails.
struct RefusingAdapter {
    peer: Peer,
}

impl RefusingAdapter {
    fn new() -> Self {
        let mut peer = Peer::new(Protocol::QuickShare, "refusing peer", "fingerprint");
        peer.address = Some("127.0.0.1:1".parse().unwrap());
        Self { peer }
    }
}

#[async_trait]
impl ProtocolAdapter for RefusingAdapter {
    fn protocol(&self) -> Protocol {
        Protocol::QuickShare
    }

    async fn start(&self, events: EventSender) -> AdapterResult<()> {
        events
            .send(AdapterEvent::Discovered(self.peer.clone()))
            .await
            .map_err(|_| AdapterError::Startup {
                protocol: self.protocol(),
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
        Err(AdapterError::Transfer {
            protocol: Protocol::QuickShare,
            reason: "the peer rejected the transfer".to_owned(),
        })
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

/// A refused transfer is not reported as a sent file.
///
/// The command used to print "sent N file(s)" as soon as the request was
/// handed to the engine, so a peer that refused the transfer looked like a
/// peer that took the file.
#[tokio::test]
async fn a_refused_transfer_is_not_reported_as_a_sent_file() {
    let mut runtime = Runtime::new(vec![Arc::new(RefusingAdapter::new())]);
    runtime.start().await;

    let files = vec![std::env::temp_dir().join("satella-check.txt")];
    let result = satella_cli::send_with(&mut runtime, "fingerprint", &files).await;

    let error = result.expect_err("a refused transfer must not be reported as a success");
    // The report names the reason, so the whole chain is checked: the
    // context alone says only that the send failed.
    let report = format!("{error:#}");
    assert!(
        report.contains("the peer rejected the transfer"),
        "the report must say why the transfer failed, got: {report}"
    );
}
