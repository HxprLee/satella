//! The receipt of an outgoing transfer.
//!
//! A send request is handed to the engine over a channel, so the caller
//! cannot watch the session it started. The engine therefore answers over a
//! one-shot channel: the caller waits for the receipt, and the receipt says
//! what really happened to the transfer.
//!
//! The receipt is kept in a registry rather than inside [`SendInfo`], because
//! `SendInfo` is serializable and its TypeScript binding is generated from it.
//! A one-shot sender is neither, and a registry keyed by the transfer id
//! leaves that binding alone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

/// What happened to an outgoing transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    /// The peer agreed and every file was sent.
    Completed,
    /// The peer refused the transfer.
    Rejected,
    /// The peer did not answer in time.
    TimedOut,
    /// The session ended before the transfer was over.
    Failed(String),
}

/// The registry of the pending receipts.
///
/// The key is the id of the transfer, which the caller picks and therefore
/// knows. A receipt that nobody waits for is dropped, so a caller that goes
/// away does not leave the registry growing.
#[derive(Debug, Default)]
pub struct OutcomeRegistry {
    pending: Mutex<HashMap<String, oneshot::Sender<SendOutcome>>>,
}

impl OutcomeRegistry {
    /// Builds an empty registry.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Registers the receipt of a transfer and hands back the waiting end.
    pub fn register(self: &Arc<Self>, id: &str) -> oneshot::Receiver<SendOutcome> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.to_owned(), tx);
        rx
    }

    /// Delivers the outcome of a transfer.
    ///
    /// A transfer nobody waits for has no receipt left, and that is not an
    /// error: the caller gave up on it.
    pub fn complete(&self, id: &str, outcome: SendOutcome) {
        if let Some(tx) = self.pending.lock().unwrap().remove(id) {
            let _ = tx.send(outcome);
        }
    }
}
