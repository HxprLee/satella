//! The handling of an incoming transfer.
//!
//! A transfer arrives and waits for a decision. The decision comes from the
//! user: the daemon asks through a desktop notification, the foreground
//! command takes every transfer it is given. Either way the decision and the
//! adapter call live here, so both paths behave the same way.

use std::path::Path;

use satella_core::peer::{FileEntry, ReceivePending};
use satella_runtime::Runtime;

use crate::notify::Decision;

/// What happened to an incoming transfer.
#[derive(Clone, Debug)]
pub struct TransferOutcome {
    /// Whether the transfer was accepted by the adapter.
    pub accepted: bool,
    /// Why the transfer failed, when it did.
    pub error: Option<String>,
    /// The files the sender offered.
    pub files: Vec<FileEntry>,
}

/// Handles one incoming transfer.
///
/// The decision tells the adapter what to do. An accepted transfer is written
/// into `destination`; a refused one is turned down at the sender.
///
/// Every protocol answers a decision: LocalSend and Quick Share both wait for
/// the application, and the MTA engine waits for the answer the adapter holds.
pub async fn handle_pending(
    runtime: &Runtime,
    pending: &ReceivePending,
    destination: &Path,
    decision: Decision,
) -> TransferOutcome {
    let files = pending.files.clone();

    let result = match decision {
        Decision::Accept => {
            runtime
                .accept(pending.clone(), destination.to_path_buf())
                .await
        }
        Decision::Refuse => runtime.reject(pending.clone()).await,
    };

    match result {
        Ok(()) => TransferOutcome {
            accepted: decision == Decision::Accept,
            error: None,
            files,
        },
        Err(err) => TransferOutcome {
            accepted: false,
            error: Some(err.to_string()),
            files,
        },
    }
}
