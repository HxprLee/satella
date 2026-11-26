//! The tracing setup of the harness.
//!
//! The foreground commands log to the terminal. The daemon logs to a file,
//! because nobody is watching its terminal anymore.

use std::fs::OpenOptions;
use std::path::Path;

use anyhow::{Context, Result};

/// The log filter used when `RUST_LOG` says nothing.
const DEFAULT_FILTER: &str = "warn,mdns_sd=off";

/// The log filter the daemon uses when `RUST_LOG` says nothing.
///
/// The daemon has nobody watching its terminal, so its log file is the only
/// record of what it did. It therefore keeps the informational lines, which
/// the foreground commands leave out.
const DAEMON_FILTER: &str = "info,mdns_sd=off";

/// Builds the log filter, honouring `RUST_LOG` when it is set.
pub fn filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_FILTER))
}

/// Builds the daemon's log filter, honouring `RUST_LOG` when it is set.
pub fn daemon_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DAEMON_FILTER))
}

/// Sends the log to a file, appending to it.
///
/// The file is created when it is missing, so a daemon that is restarted
/// keeps the log of the run before it.
pub fn init_to_file(path: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("could not open the log file {}", path.display()))?;

    tracing_subscriber::fmt()
        .with_env_filter(daemon_filter())
        .with_writer(file)
        .init();

    Ok(())
}
