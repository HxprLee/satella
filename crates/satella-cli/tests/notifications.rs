//! The checks that the daemon asks the user the right way.
//!
//! The daemon shows a desktop notification with an "Accept" and a "Refuse"
//! button for every incoming transfer, and the click decides what happens.
//! These checks pin the parts that can go wrong quietly: a click that reaches
//! the wrong transfer, a notification that never comes down, a transfer that
//! dies while the question about it is still up, and a flag that does the
//! opposite of what it says.

use satella_cli::daemon::Awaiting;
use satella_cli::notify::Decision;

use satella_core::peer::{FileEntry, Peer, ReceivePending};
use satella_core::Protocol;

/// Builds an incoming transfer.
fn pending(transfer_id: &str, alias: &str) -> ReceivePending {
    ReceivePending {
        protocol: Protocol::LocalSend,
        transfer_id: transfer_id.to_owned(),
        sender: Peer::new(Protocol::LocalSend, alias, format!("fingerprint-{alias}")),
        files: vec![FileEntry {
            name: "hello.txt".to_owned(),
            size: 12,
            mime: None,
            sha256: None,
        }],
        total_size: 12,
    }
}

/// A click reaches the transfer its notification was about.
#[test]
fn a_click_reaches_the_transfer_it_was_about() {
    let mut awaiting = Awaiting::new();
    awaiting.insert(7, pending("transfer-1", "phone"));

    let taken = awaiting.take(7).expect("the click must find its transfer");

    assert_eq!(taken.transfer_id, "transfer-1");
    assert!(awaiting.is_empty(), "a decided transfer waits no longer");
}

/// A click on a notification the daemon does not know about belongs to a
/// transfer that is already gone, so it decides nothing.
#[test]
fn a_click_on_an_unknown_notification_decides_nothing() {
    let mut awaiting = Awaiting::new();
    awaiting.insert(7, pending("transfer-1", "phone"));

    assert!(
        awaiting.take(99).is_none(),
        "an unknown click decides nothing"
    );
    assert_eq!(awaiting.len(), 1, "the waiting transfer is untouched");
}

/// Two transfers wait at once, each with its own notification, and each
/// click takes only its own.
#[test]
fn two_transfers_wait_at_once_and_each_click_takes_its_own() {
    let mut awaiting = Awaiting::new();
    awaiting.insert(1, pending("transfer-1", "phone"));
    awaiting.insert(2, pending("transfer-2", "tablet"));
    assert_eq!(awaiting.len(), 2);

    let first = awaiting
        .take(2)
        .expect("the second click finds its transfer");
    assert_eq!(first.transfer_id, "transfer-2");
    assert_eq!(first.sender.alias, "tablet");

    let second = awaiting
        .take(1)
        .expect("the first click finds its transfer");
    assert_eq!(second.transfer_id, "transfer-1");
    assert_eq!(second.sender.alias, "phone");

    assert!(awaiting.is_empty());
}

/// A transfer that dies while the question about it is up takes the question
/// and the notification with it.
#[test]
fn an_expired_transfer_takes_its_question_with_it() {
    let mut awaiting = Awaiting::new();
    awaiting.insert(7, pending("transfer-1", "phone"));
    awaiting.insert(8, pending("transfer-2", "tablet"));

    let notification_id = awaiting
        .take_expired("transfer-2")
        .expect("the expired transfer has a notification");

    assert_eq!(notification_id, 8, "the right notification comes down");
    assert_eq!(awaiting.len(), 1, "the other transfer still waits");

    // The question is gone, so a later click on it decides nothing.
    assert!(awaiting.take(8).is_none());
}

/// An expiry for a transfer the daemon does not know about changes nothing.
#[test]
fn an_unknown_expiry_changes_nothing() {
    let mut awaiting = Awaiting::new();
    awaiting.insert(7, pending("transfer-1", "phone"));

    assert!(awaiting.take_expired("transfer-9").is_none());
    assert_eq!(awaiting.len(), 1, "the waiting transfer is untouched");
}

/// The daemon asks by default and only takes the transfer when it is told
/// to, so the flag has to be the other way round from what it used to be.
#[test]
fn the_daemon_asks_by_default() {
    let command = satella_cli::parse_daemon(&[]).expect("the command must parse");

    let satella_cli::Command::Daemon { options, .. } = command else {
        panic!("the command must be the daemon");
    };

    assert!(
        !options.auto_accept,
        "the daemon asks the user unless it is told not to"
    );
}

/// The flag turns the asking off.
#[test]
fn the_auto_accept_flag_turns_the_asking_off() {
    let command =
        satella_cli::parse_daemon(&["--auto-accept".to_owned()]).expect("the command must parse");

    let satella_cli::Command::Daemon { options, .. } = command else {
        panic!("the command must be the daemon");
    };

    assert!(
        options.auto_accept,
        "the flag takes the transfers without asking"
    );
}

/// The two decisions are different things, so a click cannot be mistaken for
/// the other one.
#[test]
fn the_two_decisions_are_different() {
    assert_ne!(Decision::Accept, Decision::Refuse);
}
