//! The check that the LocalSend adapter really discovers a peer.
//!
//! Two adapters run in the same process: one announces itself over the
//! LocalSend multicast group, the other must find it. This is the real
//! protocol path, including the HTTP register request that confirms a peer,
//! so it proves discovery works rather than just that the code runs.

use std::sync::Arc;
use std::time::Duration;

use satella_core::adapter::{AdapterEvent, ProtocolAdapter};
use satella_core::Protocol;
use satella_localsend::{Config, LocalSendAdapter};

/// The alias the discovering adapter must find.
const PEER_ALIAS: &str = "satella-check-peer";

#[tokio::test]
async fn the_localsend_adapter_discovers_a_peer() {
    let discoverer = Arc::new(LocalSendAdapter::new(Config {
        alias: "satella-check-discoverer".to_owned(),
        ..Config::default()
    }));
    let target = Arc::new(LocalSendAdapter::new(Config {
        alias: PEER_ALIAS.to_owned(),
        ..Config::default()
    }));

    let (discoverer_tx, mut discoverer_rx) = tokio::sync::mpsc::channel::<AdapterEvent>(32);
    let (target_tx, _target_rx) = tokio::sync::mpsc::channel::<AdapterEvent>(32);

    discoverer
        .start(discoverer_tx)
        .await
        .expect("the discovering adapter must start");
    target
        .start(target_tx)
        .await
        .expect("the target adapter must start");

    // The target announces itself.
    let announcer = tokio::spawn({
        let target = target.clone();
        async move { target.announce().await }
    });

    let mut found = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);

    while tokio::time::Instant::now() < deadline {
        let Ok(Some(event)) = tokio::time::timeout_at(deadline, discoverer_rx.recv()).await else {
            break;
        };

        match event {
            AdapterEvent::Discovered(peer) | AdapterEvent::Updated(peer) => {
                if peer.alias == PEER_ALIAS {
                    found = Some(peer);
                    break;
                }
            }
            _ => continue,
        }
    }

    announcer.abort();
    discoverer.stop().await.expect("the discoverer must stop");
    target.stop().await.expect("the target must stop");

    let found = found.expect(
        "the announcing peer must be discovered; the announcement is multicast, so a \
         network that does not carry it cannot be checked here",
    );

    assert_eq!(found.protocol, Protocol::LocalSend);
    assert_eq!(found.alias, PEER_ALIAS);
    assert!(!found.fingerprint.is_empty());
    assert!(
        found.address.is_some(),
        "a confirmed peer must carry the address it was reached at"
    );
}
