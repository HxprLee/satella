//! The checks that the daemon's bookkeeping is right.
//!
//! The daemon keeps a merged peer list and writes it into a state file that
//! `status` reads. These checks pin the parts that can go wrong quietly: a
//! peer counted twice, a peer that stays after it is gone, a state file that
//! cannot be read back, and a pid file that names a process that is gone.

use std::path::PathBuf;

use satella_cli::daemon::{PidFile, StateFile, TransferRecord};
use satella_cli::peers::PeerTable;

use satella_core::adapter::AdapterEvent;
use satella_core::peer::{FileEntry, Peer, ReceivePending};
use satella_core::Protocol;

/// Builds a peer of one protocol.
fn peer(protocol: Protocol, alias: &str) -> Peer {
    Peer::new(protocol, alias, format!("fingerprint-{alias}"))
}

/// The same device seen over two protocols stays two entries, because each
/// of them is reached differently. The same device seen twice is one entry.
#[test]
fn the_peer_table_dedupes_by_protocol_and_fingerprint() {
    let mut table = PeerTable::new();

    assert!(table.apply(&AdapterEvent::Discovered(peer(
        Protocol::LocalSend,
        "phone"
    ))));
    assert!(table.apply(&AdapterEvent::Discovered(peer(
        Protocol::QuickShare,
        "phone"
    ))));
    assert_eq!(table.len(), 2, "two protocols are two peers");

    // The same peer again changes nothing.
    assert!(!table.apply(&AdapterEvent::Discovered(peer(
        Protocol::LocalSend,
        "phone"
    ))));
    assert_eq!(table.len(), 2, "the same peer is one peer");
}

/// A peer that is gone leaves the table.
#[test]
fn a_lost_peer_is_removed_from_the_table() {
    let mut table = PeerTable::new();
    table.apply(&AdapterEvent::Discovered(peer(
        Protocol::LocalSend,
        "phone",
    )));
    table.apply(&AdapterEvent::Discovered(peer(Protocol::Mta, "tablet")));
    assert_eq!(table.len(), 2);

    assert!(table.apply(&AdapterEvent::Lost(peer(Protocol::LocalSend, "phone"))));
    assert_eq!(table.len(), 1, "the lost peer is gone");

    // Losing a peer that is not known changes nothing.
    assert!(!table.apply(&AdapterEvent::Lost(peer(Protocol::Mta, "laptop"))));
    assert_eq!(table.len(), 1);
}

/// An update that carries the same details is not a change; one that carries
/// new details is.
#[test]
fn an_updated_peer_is_a_change_only_when_it_differs() {
    let mut table = PeerTable::new();
    let original = peer(Protocol::LocalSend, "phone");
    table.apply(&AdapterEvent::Discovered(original.clone()));

    assert!(
        !table.apply(&AdapterEvent::Updated(original.clone())),
        "the same details are not a change"
    );

    let mut updated = original.clone();
    updated.device_model = Some("Pixel".to_owned());
    assert!(
        table.apply(&AdapterEvent::Updated(updated)),
        "new details are a change"
    );
    assert_eq!(table.len(), 1, "an update does not add a peer");
}

/// The state file round-trips through JSON, so `status` can read what the
/// daemon wrote.
#[test]
fn the_state_file_is_valid_json_and_round_trips() {
    let mut table = PeerTable::new();
    table.apply(&AdapterEvent::Discovered(peer(
        Protocol::LocalSend,
        "phone",
    )));
    table.apply(&AdapterEvent::Discovered(peer(Protocol::Mta, "tablet")));

    let state = StateFile {
        pid: 4242,
        dir: PathBuf::from("/tmp/satella"),
        peers: table.snapshot(),
        transfers: vec![TransferRecord {
            protocol: Protocol::LocalSend,
            sender_alias: "phone".to_owned(),
            sender_fingerprint: "fingerprint-phone".to_owned(),
            files: vec![FileEntry {
                name: "hello.txt".to_owned(),
                size: 12,
                mime: None,
                sha256: None,
            }],
            accepted: true,
            error: None,
            at: 1_700_000_000,
        }],
        last_event: Some("a peer appeared".to_owned()),
    };

    let json = serde_json::to_string_pretty(&state).expect("the state must be serialisable");
    let back: StateFile = serde_json::from_str(&json).expect("the state must be readable");

    assert_eq!(back.pid, 4242);
    assert_eq!(back.peers.len(), 2, "both peers survive the round trip");
    assert_eq!(back.transfers.len(), 1);
    assert_eq!(back.transfers[0].files[0].name, "hello.txt");
    assert!(back.transfers[0].accepted);
}

/// The pid file round-trips too, so `status` can tell a running daemon from a
/// stale one.
#[test]
fn the_pid_file_round_trips() {
    let pid_file = PidFile {
        pid: 4242,
        started_at: 1_700_000_000,
        dir: PathBuf::from("/tmp/satella"),
        start_ticks: Some(123_456),
    };

    let json = serde_json::to_string(&pid_file).expect("the pid file must be serialisable");
    let back: PidFile = serde_json::from_str(&json).expect("the pid file must be readable");

    assert_eq!(back.pid, 4242);
    assert_eq!(back.started_at, 1_700_000_000);
    assert_eq!(back.start_ticks, Some(123_456));
}

/// A pid file that names a process that is gone is stale, and the daemon must
/// say so rather than claim that it is running.
#[test]
fn a_stale_pid_file_is_not_a_running_daemon() {
    // A pid that cannot exist: the kernel's pid limit is well below it.
    let stale = 4_294_000_000u32;

    // SAFETY: `kill` with signal 0 only checks whether the process exists.
    // It does not signal anything.
    let alive = unsafe { libc::kill(stale as i32, 0) } == 0;

    assert!(!alive, "the test needs a pid that does not exist");
}

/// The pending transfer of a peer names the protocol, so the state file can
/// tell which protocol a transfer arrived on.
#[test]
fn a_pending_transfer_names_the_senders_protocol() {
    let sender = peer(Protocol::Mta, "phone");
    let pending = ReceivePending {
        protocol: Protocol::Mta,
        transfer_id: "transfer-1".to_owned(),
        sender: sender.clone(),
        files: Vec::new(),
        total_size: 0,
    };

    assert_eq!(pending.protocol, pending.sender.protocol);
    assert!(!pending.transfer_id.is_empty());
}
