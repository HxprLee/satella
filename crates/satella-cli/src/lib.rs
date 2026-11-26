//! A terminal harness for the Satella engine.
//!
//! The harness runs every protocol adapter at the same time and prints one
//! merged device list, where every row carries the protocol the device was
//! found on. It is the manual counterpart of the UniFFI surface: what the
//! harness shows is what a frontend sees.
//!
//! The `daemon` command runs the same engine as a detached background
//! process, so the device keeps receiving files while nobody is watching the
//! terminal. It asks through a desktop notification before it takes a
//! transfer, unless it is told not to.
//!
//! Usage:
//!
//! ```text
//! satella-cli discover [--seconds N]   # list the peers of every protocol
//! satella-cli send <fingerprint> <file>...  # send files to a peer
//! satella-cli receive                   # accept incoming transfers
//! satella-cli daemon [--dir D] [--auto-accept]  # run in the background
//! satella-cli status                    # what the daemon is doing
//! satella-cli stop                      # stop the daemon
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use satella_core::adapter::AdapterEvent;
use satella_core::peer::{Peer, ReceivePending, SendFile, SendRequest};
use satella_runtime::Runtime;

pub mod daemon;
pub mod logging;
// The `Notify` method of the notification specification has nine arguments.
// The shape is the protocol's, not a choice.
#[allow(clippy::too_many_arguments)]
pub mod notify;
pub mod peers;
pub mod transfer;

use daemon::DaemonOptions;
use notify::Decision;
use peers::PeerTable;

/// How long a send waits for its peer to be discovered.
const PEER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The command the harness runs.
pub enum Command {
    /// List the peers of every protocol for a while.
    Discover {
        /// How many seconds to keep discovering.
        seconds: u64,
    },
    /// Send files to a peer.
    Send {
        /// The fingerprint of the peer to send to, or `any` for the first
        /// peer that announces itself.
        fingerprint: String,
        /// The files to send.
        files: Vec<PathBuf>,
    },
    /// Accept the incoming transfers.
    Receive {
        /// The directory received files are written into.
        destination: PathBuf,
    },
    /// Run the engine as a detached background process.
    Daemon {
        /// How the daemon is configured.
        options: DaemonOptions,
        /// Whether this process is the re-executed daemon child.
        child: bool,
    },
    /// Report what the daemon is doing.
    Status,
    /// Stop the daemon.
    Stop,
}

/// Runs the command the harness was asked for.
///
/// The daemon child logs into its log file instead of the terminal. It has to
/// install that subscriber before anything else is logged, because the global
/// subscriber can only be set once.
pub async fn run(command: Command) -> Result<()> {
    if let Command::Daemon {
        options,
        child: true,
    } = &command
    {
        logging::init_to_file(&daemon::log_path())
            .with_context(|| format!("could not log into {}", daemon::log_path().display()))?;
        return daemon::run(options.clone()).await;
    }

    // The vendored mDNS daemon logs an error when its service daemon is shut
    // down while an event is still queued. It happens after the work is done
    // and says nothing about the transfer, so it is not worth the noise.
    tracing_subscriber::fmt()
        .with_env_filter(logging::filter())
        .init();

    match command {
        Command::Discover { seconds } => discover(seconds).await,
        Command::Send { fingerprint, files } => send(&fingerprint, &files).await,
        Command::Receive { destination } => receive(destination).await,
        Command::Daemon {
            options,
            child: false,
            ..
        } => daemon::spawn(&options).await,
        Command::Status => daemon::status(),
        Command::Stop => daemon::stop().await,
        // The child returned above, before the runtime was built.
        Command::Daemon { child: true, .. } => unreachable!(),
    }
}

/// Parses the command line.
pub fn parse_args() -> Result<Command> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        anyhow::bail!(
            "usage: satella-cli <discover|send|receive|daemon|status|stop> [arguments]\n\
             \n\
             discover [--seconds N]      list the peers of every protocol\n\
             send <fingerprint> <file>...  send files to a peer\n\
             receive [directory]         accept the incoming transfers\n\
             daemon [--dir D] [--auto-accept] [--alias NAME]\n\
             \x20                           run the engine in the background\n\
             status                       report what the daemon is doing\n\
             stop                         stop the daemon"
        );
    };

    match command.as_str() {
        "discover" => {
            let mut seconds = 5;
            let mut args = args[1..].iter();
            while let Some(arg) = args.next() {
                if arg == "--seconds" {
                    seconds = args
                        .next()
                        .context("--seconds needs a number")?
                        .parse()
                        .context("--seconds needs a number")?;
                }
            }
            Ok(Command::Discover { seconds })
        }
        "send" => {
            let fingerprint = args
                .get(1)
                .context("send needs the fingerprint of a peer")?
                .clone();
            let files = args[2..].iter().map(PathBuf::from).collect::<Vec<_>>();
            if files.is_empty() {
                anyhow::bail!("send needs at least one file");
            }
            Ok(Command::Send { fingerprint, files })
        }
        "receive" => {
            let destination = args
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            Ok(Command::Receive { destination })
        }
        "daemon" => parse_daemon(&args[1..]),
        "status" => Ok(Command::Status),
        "stop" => Ok(Command::Stop),
        other => anyhow::bail!("unknown command {other:?}"),
    }
}

/// Parses the arguments of the `daemon` command.
pub fn parse_daemon(args: &[String]) -> Result<Command> {
    let mut options = DaemonOptions::default();
    let mut child = false;
    let mut args = args.iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" => {
                options.dir = PathBuf::from(args.next().context("--dir needs a path")?);
            }
            "--alias" => {
                options.alias = Some(args.next().context("--alias needs a name")?.clone());
            }
            "--auto-accept" => options.auto_accept = true,
            daemon::CHILD_FLAG => {
                // The flag alone is not enough to reach the daemon body: the
                // environment marker has to be there too, so that a user who
                // types the flag by hand does not end up in a hidden process.
                child = std::env::var(daemon::CHILD_ENV).as_deref() == Ok("1");
                if !child {
                    anyhow::bail!("{} is not a command you can run", daemon::CHILD_FLAG);
                }
            }
            other => anyhow::bail!("unknown daemon argument {other:?}"),
        }
    }

    Ok(Command::Daemon { options, child })
}

/// Builds the runtime with every adapter.
///
/// The alias is the name this device announces itself with. The directory is
/// where the adapters write the received files; `None` leaves every adapter
/// with its own default.
///
/// The MTA engine is the one adapter that needs to know whether anybody is
/// going to answer: it accepts a transfer itself unless it is told to wait
/// for the application's decision.
pub fn build_runtime(
    alias: Option<&str>,
    directory: Option<&std::path::Path>,
    mta_auto_accept: bool,
) -> Runtime {
    let mut localsend_config = satella_localsend::Config::default();
    if let Some(alias) = alias {
        localsend_config.alias = alias.to_owned();
    }

    let mut mta_config = satella_mta::Config::default();
    if let Some(alias) = alias {
        mta_config.device_name = alias.to_owned();
    }
    if let Some(directory) = directory {
        mta_config.output_dir = directory.to_path_buf();
    }
    mta_config.auto_accept = mta_auto_accept;

    let mut quickshare_config = satella_quickshare::Config::default();
    if let Some(directory) = directory {
        quickshare_config.download_path = Some(directory.to_path_buf());
    }

    let adapters: Vec<Arc<dyn satella_core::adapter::ProtocolAdapter>> = vec![
        Arc::new(satella_localsend::LocalSendAdapter::new(localsend_config)),
        Arc::new(satella_quickshare::QuickShareAdapter::new(
            quickshare_config,
        )),
        Arc::new(satella_mta::MtaAdapter::new(mta_config)),
    ];

    Runtime::new(adapters)
}

/// Prints the peers of every protocol.
async fn discover(seconds: u64) -> Result<()> {
    let mut runtime = build_runtime(None, None, true);
    let failures = runtime.start().await;
    for failure in &failures {
        eprintln!("warning: {failure}");
    }

    println!("running protocols: {}", list_protocols(&runtime));

    // The peers are collected from the merged event stream, so the list
    // covers every protocol at once.
    let mut table = PeerTable::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut printed = 0;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        let event = match tokio::time::timeout(remaining, runtime.next_event()).await {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(_) => break,
        };

        let changed = table.apply(&event);

        // The list is printed when it changes, not on every event: a peer
        // that is confirmed and then updated would otherwise reprint the
        // whole table.
        if changed && table.len() != printed {
            printed = table.len();
            print_peers(&table);
        }
    }

    print_peers(&table);
    runtime.stop().await;
    Ok(())
}

/// Sends files to a peer, found by its fingerprint.
async fn send(fingerprint: &str, files: &[PathBuf]) -> Result<()> {
    let mut runtime = build_runtime(None, None, true);
    let failures = runtime.start().await;
    for failure in &failures {
        eprintln!("warning: {failure}");
    }

    // The peer is looked up in the merged event stream, so the fingerprint is
    // looked up as the peer announces itself. A peer that is not found within
    // the timeout is reported, not silently skipped.
    let peer = wait_for_peer(&mut runtime, fingerprint, PEER_TIMEOUT).await?;

    println!(
        "sending to {} over {}",
        peer.alias,
        peer.protocol.display_name()
    );

    let request = SendRequest {
        peer: peer.clone(),
        files: files
            .iter()
            .map(|path| SendFile::Path(path.clone()))
            .collect(),
    };

    runtime.send(request).await.context("the send failed")?;
    println!("sent {} file(s)", files.len());

    runtime.stop().await;
    Ok(())
}

/// Waits for the peer with the given fingerprint to be discovered.
///
/// The fingerprint `any` matches the first peer of the wanted protocol. Some
/// protocols use a randomized address that rotates, so the address a peer had
/// a moment ago may be stale by the time the send starts.
async fn wait_for_peer(
    runtime: &mut Runtime,
    fingerprint: &str,
    timeout: std::time::Duration,
) -> Result<Peer> {
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if let Some(peer) = runtime
            .peers()
            .into_iter()
            .find(|peer| fingerprint == "any" || peer.fingerprint == fingerprint)
        {
            return Ok(peer);
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, runtime.next_event()).await {
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => break,
        }
    }

    anyhow::bail!("no peer with the fingerprint {fingerprint:?} was discovered")
}

/// Accepts the incoming transfers.
async fn receive(destination: PathBuf) -> Result<()> {
    let mut runtime = build_runtime(None, None, true);
    let failures = runtime.start().await;
    for failure in &failures {
        eprintln!("warning: {failure}");
    }

    println!(
        "waiting for incoming transfers, writing into {}",
        destination.display()
    );

    while let Some(event) = runtime.next_event().await {
        let AdapterEvent::ReceivePending(pending) = event else {
            continue;
        };

        print_pending(&pending);

        let outcome =
            transfer::handle_pending(&runtime, &pending, &destination, Decision::Accept).await;
        match &outcome.error {
            Some(error) => eprintln!("could not accept the transfer: {error}"),
            None => println!("accepted"),
        }
    }

    runtime.stop().await;
    Ok(())
}

/// The protocols of the runtime, as a readable list.
fn list_protocols(runtime: &Runtime) -> String {
    runtime
        .protocols()
        .iter()
        .map(|p| p.display_name())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Prints the merged peer list, one row per peer with its protocol.
fn print_peers(table: &PeerTable) {
    if table.is_empty() {
        println!("no peers found yet");
        return;
    }

    println!();
    println!("{:<14} {:<24} {:<40}", "PROTOCOL", "ALIAS", "FINGERPRINT");
    for peer in table.snapshot() {
        println!(
            "{:<14} {:<24} {:<40}",
            peer.protocol.display_name(),
            truncate(&peer.alias, 24),
            peer.fingerprint,
        );
    }
}

/// Prints a pending transfer, naming the protocol the sender used.
fn print_pending(pending: &ReceivePending) {
    println!();
    println!(
        "incoming transfer over {} from {}",
        pending.protocol.display_name(),
        pending.sender.alias,
    );
    for file in &pending.files {
        println!("  {} ({} bytes)", file.name, file.size);
    }
}

/// Shortens a string to the given width.
fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}
