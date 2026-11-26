//! The checks of the outcome of an outgoing Quick Share transfer.
//!
//! A send that answers before the peer said anything makes a refused
//! transfer look like a successful one. These checks drive the adapter
//! against a peer that refuses the transfer and against a peer that never
//! answers, and they check that both reach the caller as a failure.
//!
//! The peers here are fake on purpose: they are a TCP listener, so the check
//! exercises the engine's own timeout and its own refusal handling, not the
//! protocol of a real phone.

use std::net::SocketAddr;
use std::time::Duration;

use satella_core::adapter::{AdapterError, ProtocolAdapter};
use satella_core::peer::{Peer, SendFile, SendRequest};
use satella_core::Protocol;
use satella_quickshare::{Config, QuickShareAdapter};

/// A peer that accepts the connection and then says nothing at all.
///
/// The engine has to give up on it: a peer that never answers would keep the
/// session pending forever.
async fn a_silent_peer() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            // The socket is held open without a word, so the engine has to
            // be the one that gives up.
            held.push(socket);
        }
    });

    addr
}

/// A peer that accepts the connection and hangs up straight away.
async fn a_peer_that_hangs_up() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            drop(socket);
        }
    });

    addr
}

/// The peer the adapter is pointed at.
fn peer_at(addr: SocketAddr) -> Peer {
    let mut peer = Peer::new(Protocol::QuickShare, "silent peer", "fingerprint");
    peer.address = Some(addr);
    peer
}

/// A send to a peer that never answers must fail.
///
/// The engine used to answer the send as soon as the request was queued, so
/// the caller never heard that the peer was silent.
#[tokio::test]
async fn a_send_to_a_peer_that_never_answers_fails() {
    let addr = a_silent_peer().await;

    let adapter = QuickShareAdapter::new(Config {
        // The wait is short, so the check does not take half a minute.
        consent_timeout: Some(Duration::from_secs(2)),
        ..Config::default()
    });
    let (tx, _rx) = tokio::sync::mpsc::channel(32);
    adapter.start(tx).await.expect("the adapter must start");

    let request = SendRequest {
        peer: peer_at(addr),
        files: vec![SendFile::Path(
            std::env::temp_dir().join("satella-check.txt"),
        )],
    };

    // The engine gives up on its own, so the check waits for that rather
    // than for a peer that will never answer.
    let result = tokio::time::timeout(Duration::from_secs(30), adapter.send(request)).await;

    adapter.stop().await.expect("the adapter must stop");

    let result = result.expect("the send must not wait forever");
    let error = result.expect_err("a peer that never answers must not be a successful send");
    assert!(
        matches!(
            error,
            AdapterError::Transfer { .. } | AdapterError::Unreachable { .. }
        ),
        "the send must fail, got: {error:?}"
    );
}

/// A send to a peer that hangs up must fail.
///
/// A peer that drops the connection did not take the files, so the send must
/// not be reported as a success.
#[tokio::test]
async fn a_send_to_a_peer_that_hangs_up_fails() {
    let addr = a_peer_that_hangs_up().await;

    let adapter = QuickShareAdapter::new(Config::default());
    let (tx, _rx) = tokio::sync::mpsc::channel(32);
    adapter.start(tx).await.expect("the adapter must start");

    let request = SendRequest {
        peer: peer_at(addr),
        files: vec![SendFile::Path(
            std::env::temp_dir().join("satella-check.txt"),
        )],
    };

    let result = tokio::time::timeout(Duration::from_secs(60), adapter.send(request)).await;

    adapter.stop().await.expect("the adapter must stop");

    let result = result.expect("the send must not wait forever");
    let error = result.expect_err("a peer that hangs up must not be a successful send");
    assert!(
        matches!(
            error,
            AdapterError::Transfer { .. } | AdapterError::Unreachable { .. }
        ),
        "the send must fail, got: {error:?}"
    );
}
