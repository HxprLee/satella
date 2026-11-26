//! The merged peer table.
//!
//! Every protocol reports its own peers, and the same physical device may be
//! reachable over more than one protocol. The table keeps one entry per
//! protocol and fingerprint, so a device seen twice stays two rows: each of
//! them is reached differently.
//!
//! The table is shared by `discover`, which prints it, and by the daemon,
//! which keeps it up to date and writes it into its state file. One
//! implementation, so the two cannot drift.

use std::collections::HashMap;

use satella_core::adapter::AdapterEvent;
use satella_core::peer::Peer;
use satella_core::Protocol;

/// The peers known across every protocol.
#[derive(Default)]
pub struct PeerTable {
    peers: HashMap<(Protocol, String), Peer>,
}

impl PeerTable {
    /// Builds an empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one discovery event to the table.
    ///
    /// Returns whether the table changed. A peer that is confirmed and then
    /// updated is a change in the peer's details but not in the set of peers,
    /// so the caller can decide what "changed" means for its own output.
    pub fn apply(&mut self, event: &AdapterEvent) -> bool {
        match event {
            AdapterEvent::Discovered(peer) | AdapterEvent::Updated(peer) => {
                let key = (peer.protocol, peer.fingerprint.clone());
                match self.peers.get(&key) {
                    Some(known) if known == peer => false,
                    _ => {
                        self.peers.insert(key, peer.clone());
                        true
                    }
                }
            }
            AdapterEvent::Lost(peer) => {
                let key = (peer.protocol, peer.fingerprint.clone());
                self.peers.remove(&key).is_some()
            }
            _ => false,
        }
    }

    /// The peers, sorted by protocol and alias.
    pub fn snapshot(&self) -> Vec<Peer> {
        let mut peers: Vec<Peer> = self.peers.values().cloned().collect();
        peers.sort_by(|a, b| {
            a.protocol
                .cmp(&b.protocol)
                .then_with(|| a.alias.cmp(&b.alias))
        });
        peers
    }

    /// Whether no peer is known.
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// How many peers are known.
    pub fn len(&self) -> usize {
        self.peers.len()
    }
}
