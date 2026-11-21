//! A terminal harness for the Satella engine.
//!
//! The harness runs every protocol adapter at the same time and prints one
//! merged device list, where every row carries the protocol the device was
//! found on. It is the manual counterpart of the UniFFI surface: what the
//! harness shows is what a frontend sees.
//!
//! Usage:
//!
//! ```text
//! satella-cli discover [--seconds N]   # list the peers of every protocol
//! satella-cli send <fingerprint> <file>...  # send files to a peer
//! satella-cli receive                   # accept incoming transfers
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use satella_core::adapter::{AdapterEvent, ProtocolAdapter};
use satella_core::peer::{Peer, ReceivePending, SendFile, SendRequest};
use satella_core::Protocol;
use satella_runtime::Runtime;

/// How long a send waits for its peer to be discovered.
const PEER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The command the harness runs.
enum Command {
    /// List the peers of every protocol for a while.
    Discover {
        /// How many seconds to keep discovering.
        seconds: u64,
    },
    /// Send files to a peer.
    Send {
        /// The fingerprint of the peer to send to.
        fingerprint: String,
        /// The files to send.
        files: Vec<PathBuf>,
    },
    /// Accept the incoming transfers.
    Receive {
        /// The directory received files are written into.
        destination: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // The vendored mDNS daemon logs an error when its service daemon is shut
    // down while an event is still queued. It happens after the work is done
    // and says nothing about the transfer, so it is not worth the noise.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,mdns_sd=off"));

    tracing_subscriber::fmt().with_env_filter(filter).init();

    let command = parse_args()?;
    let runtime = build_runtime();

    match command {
        Command::Discover { seconds } => discover(runtime, seconds).await,
        Command::Send { fingerprint, files } => send(runtime, &fingerprint, &files).await,
        Command::Receive { destination } => receive(runtime, destination).await,
    }
}

/// Parses the command line.
fn parse_args() -> Result<Command> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        anyhow::bail!(
            "usage: satella-cli <discover|send|receive> [arguments]\n\
             \n\
             discover [--seconds N]      list the peers of every protocol\n\
             send <fingerprint> <file>...  send files to a peer\n\
             receive [directory]         accept the incoming transfers"
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
        other => anyhow::bail!("unknown command {other:?}"),
    }
}

/// Builds the runtime with every adapter.
fn build_runtime() -> Runtime {
    let adapters: Vec<Arc<dyn ProtocolAdapter>> = vec![
        Arc::new(satella_localsend::LocalSendAdapter::new(
            satella_localsend::Config::default(),
        )),
        Arc::new(satella_quickshare::QuickShareAdapter::new(
            satella_quickshare::Config::default(),
        )),
        Arc::new(satella_mta::MtaAdapter::new(satella_mta::Config::default())),
    ];

    Runtime::new(adapters)
}

/// Prints the peers of every protocol.
async fn discover(mut runtime: Runtime, seconds: u64) -> Result<()> {
    let failures = runtime.start().await;
    for failure in &failures {
        eprintln!("warning: {failure}");
    }

    println!("running protocols: {}", list_protocols(&runtime));

    // The peers are collected from the merged event stream, so the list
    // covers every protocol at once.
    let mut peers: HashMap<(Protocol, String), Peer> = HashMap::new();
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

        let changed = match event {
            AdapterEvent::Discovered(peer) | AdapterEvent::Updated(peer) => {
                peers.insert((peer.protocol, peer.fingerprint.clone()), peer);
                true
            }
            AdapterEvent::Lost(peer) => {
                peers.remove(&(peer.protocol, peer.fingerprint.clone()));
                true
            }
            _ => false,
        };

        // The list is printed when it changes, not on every event: a peer
        // that is confirmed and then updated would otherwise reprint the
        // whole table.
        if changed && peers.len() != printed {
            printed = peers.len();
            print_peers(&peers);
        }
    }

    print_peers(&peers);
    runtime.stop().await;
    Ok(())
}

/// Sends files to a peer, found by its fingerprint.
async fn send(mut runtime: Runtime, fingerprint: &str, files: &[PathBuf]) -> Result<()> {
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
            .find(|peer| peer.fingerprint == fingerprint)
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
async fn receive(mut runtime: Runtime, destination: PathBuf) -> Result<()> {
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

        match runtime.accept(pending.clone(), destination.clone()).await {
            Ok(()) => println!("accepted"),
            Err(err) => eprintln!("could not accept the transfer: {err}"),
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
fn print_peers(peers: &HashMap<(Protocol, String), Peer>) {
    if peers.is_empty() {
        println!("no peers found yet");
        return;
    }

    let mut rows: Vec<&Peer> = peers.values().collect();
    rows.sort_by(|a, b| {
        a.protocol
            .cmp(&b.protocol)
            .then_with(|| a.alias.cmp(&b.alias))
    });

    println!();
    println!("{:<14} {:<24} {:<40}", "PROTOCOL", "ALIAS", "FINGERPRINT");
    for peer in rows {
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
