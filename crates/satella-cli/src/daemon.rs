//! The daemon: a detached process that keeps every protocol adapter running.
//!
//! The daemon is the same engine the foreground commands use, started as a
//! background process. It keeps the peer list and the transfer history up to
//! date and writes them into a state file, so `status` can report them
//! without talking to the running process.
//!
//! The daemon detaches by re-executing itself: the parent sets `setsid` and
//! execs the binary again with a hidden flag, and the new process runs the
//! daemon body. `fork` is not used, because the Tokio runtime of this process
//! already has worker threads and a forked child would inherit a broken
//! runtime.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use satella_core::adapter::AdapterEvent;
use satella_core::peer::{FileEntry, Peer, ReceivePending};
use satella_core::Protocol;

use crate::notify::{Decision, Notifier};
use crate::peers::PeerTable;
use crate::transfer::handle_pending;

/// The flag the parent passes to the process it re-executes.
pub const CHILD_FLAG: &str = "--daemon-child";

/// The environment marker that says this process is the re-executed child.
///
/// The flag alone is not enough: a user who types it by hand must not land in
/// the daemon body.
pub const CHILD_ENV: &str = "SATELLA_DAEMON_CHILD";

/// The environment variable that names the file descriptor the child reports
/// its readiness on.
const READY_FD_ENV: &str = "SATELLA_DAEMON_READY_FD";

/// How long the parent waits for the daemon to report that it is up.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long `stop` waits for the daemon to exit before killing it.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// How many transfers the state file keeps.
const MAX_TRANSFERS: usize = 50;

/// How the daemon is configured.
#[derive(Clone, Debug)]
pub struct DaemonOptions {
    /// The directory the received files are written into.
    pub dir: PathBuf,
    /// Whether an incoming transfer is taken without asking.
    ///
    /// The daemon asks through a desktop notification by default. This is the
    /// way back to taking every transfer without a question.
    pub auto_accept: bool,
    /// The name this device announces itself with.
    pub alias: Option<String>,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            dir: default_dir(),
            auto_accept: false,
            alias: None,
        }
    }
}

/// The directory the daemon writes the received files into by default.
///
/// It is the same directory the MTA adapter writes into, so the daemon drops
/// the files where the user already expects them.
pub fn default_dir() -> PathBuf {
    satella_mta::Config::default().output_dir
}

/// The PID file of the running daemon.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PidFile {
    /// The process id of the daemon.
    ///
    /// It is unsigned, because a process id is never negative; the kernel's
    /// own limit is far below the point where the type would overflow.
    pub pid: u32,
    /// When the daemon was started, in seconds since the epoch.
    pub started_at: u64,
    /// The directory the received files are written into.
    pub dir: PathBuf,
    /// The start time of the daemon's process, in clock ticks since the boot.
    ///
    /// It is what tells a recycled PID from the daemon that owns the file.
    #[serde(default)]
    pub start_ticks: Option<u64>,
}

/// One incoming transfer the daemon saw.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TransferRecord {
    /// The protocol the sender used.
    pub protocol: Protocol,
    /// The name of the sender.
    pub sender_alias: String,
    /// The fingerprint of the sender.
    pub sender_fingerprint: String,
    /// The files the sender offered.
    pub files: Vec<FileEntry>,
    /// Whether the transfer was accepted.
    pub accepted: bool,
    /// Why the transfer failed, when it did.
    pub error: Option<String>,
    /// When the transfer was announced, in seconds since the epoch.
    pub at: u64,
}

/// What the daemon knows about itself.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StateFile {
    /// The process id of the daemon.
    pub pid: u32,
    /// The directory the received files are written into.
    pub dir: PathBuf,
    /// The peers of every protocol.
    pub peers: Vec<Peer>,
    /// The transfers the daemon saw, oldest first.
    pub transfers: Vec<TransferRecord>,
    /// What the daemon did last.
    pub last_event: Option<String>,
}

/// The directory the daemon keeps its files in.
pub fn state_dir() -> PathBuf {
    dirs::cache_dir()
        .or_else(dirs::runtime_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("satella-cli")
}

/// The path of the PID file.
pub fn pid_path() -> PathBuf {
    state_dir().join("satella-cli.pid")
}

/// The path of the state file.
pub fn state_path() -> PathBuf {
    state_dir().join("satella-cli.state.json")
}

/// The path of the log file.
pub fn log_path() -> PathBuf {
    state_dir().join("satella-cli.log")
}

/// Starts the daemon and waits until it reports that it is up.
///
/// The parent does not do any work itself: it sets `setsid`, re-executes this
/// binary with the hidden child flag, and waits for the child to say that it
/// started. That way the parent never prints "started" for a daemon that then
/// dies.
pub async fn spawn(options: &DaemonOptions) -> Result<()> {
    if let Some(pid_file) = read_pid_file()? {
        match liveness(pid_file.pid) {
            Liveness::Alive => bail!(
                "the daemon is already running (pid {}); stop it first",
                pid_file.pid
            ),
            Liveness::Foreign => bail!(
                "the pid file names pid {}, which belongs to another user",
                pid_file.pid
            ),
            Liveness::NotRunning => {
                // The file is left over from a daemon that died. It is
                // replaced below.
            }
        }
    }

    std::fs::create_dir_all(state_dir())
        .with_context(|| format!("could not create {}", state_dir().display()))?;

    // A pid file that cannot be read names no process, so it is in the way of
    // nothing but the daemon that writes its own.
    if pid_file_is_corrupt() {
        let _ = std::fs::remove_file(pid_path());
    }

    let exe = std::env::current_exe().context("could not find this executable")?;

    // The child reports its readiness on one end of a socket pair. A socket
    // is used instead of a pipe because the parent can wait on it with a
    // timeout, and because the read end sees the end of the file when the
    // child dies.
    let (read_fd, write_fd) = socket_pair()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
        .with_context(|| format!("could not open {}", log_path().display()))?;

    let mut command = tokio::process::Command::new(exe);
    command
        .arg("daemon")
        .arg(CHILD_FLAG)
        .arg("--dir")
        .arg(&options.dir)
        .env(CHILD_ENV, "1")
        .env(READY_FD_ENV, write_fd.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));

    if options.auto_accept {
        command.arg("--auto-accept");
    }
    if let Some(alias) = &options.alias {
        command.arg("--alias").arg(alias);
    }

    // SAFETY: the closure runs in the forked child, between fork and exec,
    // where only one thread exists. It calls `setsid`, which is async-signal
    // safe, and touches no state of this process.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                // The process is already a group leader. Falling back to a new
                // process group still detaches it from the terminal.
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }

    let child = command.spawn().context("could not start the daemon")?;
    // The parent must not hold the writing end open, or the read end would
    // never see the end of the file when the child dies.
    // SAFETY: the descriptor is owned by this function and not used again.
    unsafe { libc::close(write_fd) };

    let ready = wait_for_ready(read_fd).await;
    // The handle is dropped on purpose: the daemon outlives this process.
    drop(child);

    match ready {
        Ok(pid) => {
            println!("satella-cli daemon started (pid {pid})");
            println!("  log:    {}", log_path().display());
            println!("  state:  {}", state_path().display());
            println!("  files:  {}", options.dir.display());
            Ok(())
        }
        Err(err) => {
            bail!(
                "the daemon did not start: {err:#}\nthe log is {}",
                log_path().display()
            )
        }
    }
}

/// Waits for the child to report that it is up.
///
/// Returns the process id of the daemon.
async fn wait_for_ready(read_fd: RawFd) -> Result<i32> {
    // SAFETY: the descriptor was just created by `socket_pair` and is not
    // used anywhere else.
    let stream = unsafe { StdUnixStream::from_raw_fd(read_fd) };
    stream
        .set_nonblocking(true)
        .context("could not make the readiness socket non-blocking")?;

    let mut stream =
        tokio::net::UnixStream::from_std(stream).context("could not use the readiness socket")?;

    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(READY_TIMEOUT, async {
        loop {
            let count = stream.read(&mut byte).await?;
            if count == 0 || byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
        }
        Ok::<(), std::io::Error>(())
    })
    .await
    .context("the daemon did not report that it started in time")?;

    read?;

    let line = String::from_utf8_lossy(&line).into_owned();
    let pid = line
        .split_whitespace()
        .nth(1)
        .and_then(|pid| pid.parse().ok())
        .with_context(|| format!("the daemon reported {line:?}, which is not a pid"))?;

    Ok(pid)
}

/// The transfers that are waiting for the user.
///
/// The key is the id of the notification that asks about the transfer, so a
/// click reaches the transfer it was about. Two transfers can wait at once,
/// each with its own notification.
#[derive(Debug, Default)]
pub struct Awaiting {
    transfers: HashMap<u32, ReceivePending>,
}

impl Awaiting {
    /// Builds an empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Remembers a transfer that is waiting for a decision.
    pub fn insert(&mut self, notification_id: u32, pending: ReceivePending) {
        self.transfers.insert(notification_id, pending);
    }

    /// Takes the transfer a click was about.
    ///
    /// A click on a notification the daemon does not know about belongs to a
    /// transfer that is already gone, so there is nothing to take.
    pub fn take(&mut self, notification_id: u32) -> Option<ReceivePending> {
        self.transfers.remove(&notification_id)
    }

    /// Takes the transfer that ended on its own.
    ///
    /// The notification is about a transfer that no longer exists, so it is
    /// taken off the screen as well.
    pub fn take_expired(&mut self, transfer_id: &str) -> Option<u32> {
        let notification_id = self
            .transfers
            .iter()
            .find(|(_, pending)| pending.transfer_id == transfer_id)
            .map(|(id, _)| *id)?;

        self.transfers.remove(&notification_id);
        Some(notification_id)
    }

    /// How many transfers are waiting.
    pub fn len(&self) -> usize {
        self.transfers.len()
    }

    /// Whether no transfer is waiting.
    pub fn is_empty(&self) -> bool {
        self.transfers.is_empty()
    }
}

/// Runs the daemon body.
///
/// This is the process the parent re-executes into.
pub async fn run(options: DaemonOptions) -> Result<()> {
    std::fs::create_dir_all(&options.dir)
        .with_context(|| format!("could not create {}", options.dir.display()))?;

    let mut runtime = crate::build_runtime(
        options.alias.as_deref(),
        Some(&options.dir),
        options.auto_accept,
    );
    let failures = runtime.start().await;
    for failure in &failures {
        tracing::warn!("{failure}");
    }

    // The notification service is what the user answers the daemon with. A
    // machine without a session bus cannot ask, so the daemon falls back to
    // taking the transfers instead of dropping them.
    let mut notifier = if options.auto_accept {
        None
    } else {
        match Notifier::connect().await {
            Ok(notifier) => Some(notifier),
            Err(err) => {
                tracing::warn!(
                    "could not reach the notification bus, so the transfers are taken without asking: {err}"
                );
                None
            }
        }
    };

    let pid = std::process::id();
    let pid_file = PidFile {
        pid,
        started_at: now_seconds(),
        dir: options.dir.clone(),
        start_ticks: process_start_ticks(pid),
    };
    write_pid_file(&pid_file)?;

    let mut table = PeerTable::new();
    let mut transfers: Vec<TransferRecord> = Vec::new();
    let mut state = StateFile {
        pid,
        dir: options.dir.clone(),
        peers: Vec::new(),
        transfers: Vec::new(),
        last_event: None,
    };

    tracing::info!(
        pid,
        dir = %options.dir.display(),
        auto_accept = options.auto_accept,
        notifications = notifier.is_some(),
        "the daemon is up"
    );

    // The state file is written before the readiness is reported, so that
    // `status` never finds a running daemon without a state to report.
    write_state_file(&state)?;

    // The readiness is reported only now: the pid file is written and the
    // adapters are running, so the parent can print "started" truthfully.
    report_ready(pid);

    let mut shutdown = shutdown_signal().await?;

    // The transfers that are waiting for the user, by the id of the
    // notification that asks about them.
    let mut awaiting = Awaiting::new();

    loop {
        // A click arrives on its own branch of the select, because the
        // runtime's event stream has to keep being read while the user
        // decides.
        let mut click: Option<(u32, Decision)> = None;

        let event: Option<AdapterEvent> = tokio::select! {
            event = runtime.next_event() => event,
            clicked = next_click(notifier.as_mut()) => {
                click = clicked;
                None
            }
            _ = shutdown.recv() => break,
        };

        if let Some((id, decision)) = click {
            // A click on a notification the daemon does not know about is
            // ignored: it belongs to a transfer that is already gone.
            let Some(pending) = awaiting.take(id) else {
                continue;
            };

            let outcome = handle_pending(&runtime, &pending, &options.dir, decision).await;
            transfers.push(record(&pending, &outcome));
            trim(&mut transfers);
            state.last_event = Some(format!(
                "{} transfer from {} was {}",
                pending.protocol.display_name(),
                pending.sender.alias,
                match decision {
                    Decision::Accept => "accepted",
                    Decision::Refuse => "refused",
                }
            ));

            state.peers = table.snapshot();
            state.transfers = transfers.clone();
            write_state_file(&state)?;
            continue;
        }

        // Every adapter closed its channel: there is nothing left to wait
        // for.
        let Some(event) = event else {
            break;
        };

        let changed = table.apply(&event);
        state.last_event = Some(describe(&event));

        if let AdapterEvent::ReceivePending(pending) = &event {
            if options.auto_accept || notifier.is_none() {
                // Nobody is asked, so the transfer is taken right away.
                let outcome =
                    handle_pending(&runtime, pending, &options.dir, Decision::Accept).await;
                transfers.push(record(pending, &outcome));
                trim(&mut transfers);
            } else if let Some(notifier) = &notifier {
                match notifier.ask(pending).await {
                    Ok(id) => {
                        // The notification stays up until the user clicks or
                        // the transfer dies on its own.
                        awaiting.insert(id, pending.clone());
                        tracing::info!(
                            "a transfer from {} is waiting for a decision",
                            pending.sender.alias
                        );
                    }
                    Err(err) => {
                        // The bus died under us. Taking the transfer is
                        // better than leaving the sender waiting.
                        tracing::warn!("could not show the notification: {err}");
                        let outcome =
                            handle_pending(&runtime, pending, &options.dir, Decision::Accept).await;
                        transfers.push(record(pending, &outcome));
                        trim(&mut transfers);
                    }
                }
            }

            state.last_event = Some(format!(
                "{} transfer from {}",
                pending.protocol.display_name(),
                pending.sender.alias
            ));
        }

        if let AdapterEvent::ReceiveExpired { transfer_id, .. } = &event {
            // The transfer is gone, so the question about it is withdrawn and
            // the notification is taken off the screen.
            if let Some(id) = awaiting.take_expired(transfer_id) {
                if let Some(notifier) = &notifier {
                    notifier.close(id).await;
                }
                tracing::info!("a transfer expired before it was decided");
            }
        }

        if changed
            || matches!(
                event,
                AdapterEvent::ReceivePending(_) | AdapterEvent::ReceiveExpired { .. }
            )
        {
            state.peers = table.snapshot();
            state.transfers = transfers.clone();
            write_state_file(&state)?;
        }
    }

    tracing::info!("the daemon is stopping");
    runtime.stop().await;
    remove_pid_file(pid);
    Ok(())
}

/// Waits for the next click on a notification.
///
/// A daemon that takes the transfers without asking has no notification to
/// click, so it never gets one.
async fn next_click(notifier: Option<&mut Notifier>) -> Option<(u32, Decision)> {
    match notifier {
        Some(notifier) => notifier.next_click().await,
        None => {
            // The daemon has nothing to wait for, so the future never ends.
            std::future::pending().await
        }
    }
}

/// Keeps only the newest transfers in the history.
fn trim(transfers: &mut Vec<TransferRecord>) {
    if transfers.len() > MAX_TRANSFERS {
        let overflow = transfers.len() - MAX_TRANSFERS;
        transfers.drain(0..overflow);
    }
}

/// Reports that the daemon is up, on the socket the parent waits on.
fn report_ready(pid: u32) {
    let Ok(raw) = std::env::var(READY_FD_ENV) else {
        return;
    };
    let Ok(fd) = raw.parse::<RawFd>() else {
        return;
    };

    // SAFETY: the descriptor was inherited from the parent and is used once.
    let mut socket = unsafe { File::from_raw_fd(fd) };
    let _ = writeln!(socket, "READY {pid}");
    let _ = socket.flush();
}

/// Waits for the process to be asked to stop.
///
/// Both `SIGTERM` and `SIGINT` end the daemon, so `stop` and a Ctrl-C in the
/// terminal take the same path.
async fn shutdown_signal() -> Result<tokio::sync::mpsc::Receiver<()>> {
    let (tx, rx) = tokio::sync::mpsc::channel(1);

    tokio::spawn(async move {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("the SIGTERM handler must install");
        let mut interrupt =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .expect("the SIGINT handler must install");

        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }

        let _ = tx.send(()).await;
    });

    Ok(rx)
}

/// Prints what the daemon knows about itself.
pub fn status() -> Result<()> {
    let Some(pid_file) = read_pid_file()? else {
        if pid_file_is_corrupt() {
            println!("satella-cli daemon: not running (the pid file is corrupt)");
        } else {
            println!("satella-cli daemon: not running");
        }
        return Ok(());
    };

    match liveness(pid_file.pid) {
        Liveness::NotRunning => {
            println!(
                "satella-cli daemon: not running (the pid file is left over from pid {})",
                pid_file.pid
            );
            return Ok(());
        }
        Liveness::Foreign => {
            println!(
                "satella-cli daemon: pid {} belongs to another user",
                pid_file.pid
            );
            return Ok(());
        }
        Liveness::Alive => {}
    }

    // The PID may have been reused by an unrelated program since the daemon
    // wrote the file. The start time tells the two apart.
    if let (Some(expected), Some(actual)) =
        (pid_file.start_ticks, process_start_ticks(pid_file.pid))
    {
        if expected != actual {
            println!(
                "satella-cli daemon: not running (pid {} was reused by another program)",
                pid_file.pid
            );
            return Ok(());
        }
    }

    let uptime = now_seconds().saturating_sub(pid_file.started_at);
    println!(
        "satella-cli daemon: running (pid {}, up {})",
        pid_file.pid,
        human(uptime)
    );
    println!("  files: {}", pid_file.dir.display());

    let Some(state) = read_state_file()? else {
        println!("  the state file is not there yet");
        return Ok(());
    };

    if state.peers.is_empty() {
        println!("  peers:  none yet");
    } else {
        println!();
        println!("{:<14} {:<24} {:<40}", "PROTOCOL", "ALIAS", "FINGERPRINT");
        for peer in &state.peers {
            println!(
                "{:<14} {:<24} {:<40}",
                peer.protocol.display_name(),
                truncate(&peer.alias, 24),
                peer.fingerprint
            );
        }
    }

    if !state.transfers.is_empty() {
        println!();
        println!("transfers:");
        for transfer in &state.transfers {
            let outcome = match (&transfer.error, transfer.accepted) {
                (Some(error), _) => format!("failed: {error}"),
                (None, true) => "accepted".to_owned(),
                (None, false) => "not accepted".to_owned(),
            };
            println!(
                "  {:<14} from {:<20} {} file(s), {}",
                transfer.protocol.display_name(),
                truncate(&transfer.sender_alias, 20),
                transfer.files.len(),
                outcome
            );
        }
    }

    Ok(())
}

/// Stops the running daemon.
pub async fn stop() -> Result<()> {
    let Some(pid_file) = read_pid_file()? else {
        if pid_file_is_corrupt() {
            let _ = std::fs::remove_file(pid_path());
            println!("satella-cli daemon: not running (the corrupt pid file was removed)");
        } else {
            println!("satella-cli daemon: not running");
        }
        return Ok(());
    };

    match liveness(pid_file.pid) {
        Liveness::NotRunning => {
            remove_pid_file(pid_file.pid);
            println!("satella-cli daemon: not running (the stale pid file was removed)");
            return Ok(());
        }
        Liveness::Foreign => bail!(
            "the daemon pid {} belongs to another user, so it was not stopped",
            pid_file.pid
        ),
        Liveness::Alive => {}
    }

    // Killing a recycled PID would kill an unrelated program, so the start
    // time has to match before anything is signalled.
    if let (Some(expected), Some(actual)) =
        (pid_file.start_ticks, process_start_ticks(pid_file.pid))
    {
        if expected != actual {
            bail!(
                "pid {} was reused by another program, so it was not stopped",
                pid_file.pid
            );
        }
    }

    // SAFETY: the pid was checked to be alive and to be the daemon's own.
    unsafe { libc::kill(pid_file.pid as i32, libc::SIGTERM) };

    let deadline = tokio::time::Instant::now() + STOP_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if liveness(pid_file.pid) == Liveness::NotRunning {
            remove_pid_file(pid_file.pid);
            println!("satella-cli daemon: stopped (pid {})", pid_file.pid);
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // The daemon did not leave in time. It is killed, because a daemon that
    // ignores its shutdown signal would keep the port and the identity.
    // SAFETY: the pid was checked to be alive and to be the daemon's own.
    unsafe { libc::kill(pid_file.pid as i32, libc::SIGKILL) };
    remove_pid_file(pid_file.pid);
    println!("satella-cli daemon: killed (pid {})", pid_file.pid);
    Ok(())
}

/// Writes the PID file.
fn write_pid_file(pid_file: &PidFile) -> Result<()> {
    let json = serde_json::to_string_pretty(pid_file).context("could not write the pid file")?;
    std::fs::write(pid_path(), json)
        .with_context(|| format!("could not write {}", pid_path().display()))
}

/// Reads the PID file, if it is there.
///
/// A file that cannot be read is treated as no file at all: a corrupt pid
/// file names no process, so it must not stop a daemon from being started.
fn read_pid_file() -> Result<Option<PidFile>> {
    let path = pid_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };

    match serde_json::from_str(&text) {
        Ok(pid_file) => Ok(Some(pid_file)),
        Err(err) => {
            tracing::warn!("the pid file is not readable: {err}");
            Ok(None)
        }
    }
}

/// Whether a pid file is there but cannot be read.
///
/// A corrupt pid file is left over from a daemon that died badly. It names no
/// process, so `stop` removes it instead of leaving it in the way.
fn pid_file_is_corrupt() -> bool {
    let Ok(text) = std::fs::read_to_string(pid_path()) else {
        return false;
    };
    serde_json::from_str::<PidFile>(&text).is_err()
}

/// Removes the PID file, if it still names this process.
fn remove_pid_file(pid: u32) {
    if let Ok(Some(pid_file)) = read_pid_file() {
        if pid_file.pid != pid {
            return;
        }
    }
    let _ = std::fs::remove_file(pid_path());
}

/// Writes the state file.
///
/// The file is written next to the target and renamed over it, so a reader
/// never sees half a file.
fn write_state_file(state: &StateFile) -> Result<()> {
    let json = serde_json::to_string_pretty(state).context("could not write the state file")?;
    let target = state_path();
    let temporary = target.with_extension("json.tmp");

    std::fs::write(&temporary, json)
        .with_context(|| format!("could not write {}", temporary.display()))?;
    std::fs::rename(&temporary, &target).with_context(|| {
        format!(
            "could not rename {} to {}",
            temporary.display(),
            target.display()
        )
    })
}

/// Reads the state file, if it is there and readable.
fn read_state_file() -> Result<Option<StateFile>> {
    let path = state_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };

    match serde_json::from_str(&text) {
        Ok(state) => Ok(Some(state)),
        Err(err) => {
            tracing::warn!("the state file is not readable: {err}");
            Ok(None)
        }
    }
}

/// What the daemon does with a process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Liveness {
    /// The process exists and belongs to this user.
    Alive,
    /// The process does not exist.
    NotRunning,
    /// The process exists, but belongs to another user.
    Foreign,
}

/// Checks whether a process exists.
fn liveness(pid: u32) -> Liveness {
    // SAFETY: `kill` with signal 0 only checks whether the process exists.
    // It does not signal anything.
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return Liveness::Alive;
    }

    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Liveness::NotRunning,
        Some(libc::EPERM) => Liveness::Foreign,
        _ => Liveness::NotRunning,
    }
}

/// The start time of a process, in clock ticks since the boot.
///
/// It is what tells a recycled PID from the process that owns the PID file.
/// The value comes from `/proc`, which Linux has and the other systems do
/// not; there the check is skipped.
fn process_start_ticks(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;

    // The command name is in parentheses and may contain spaces, so the
    // fields are counted after the last ')'. The start time is the 20th
    // field after it.
    let after = stat.rsplit_once(')')?.1;
    after.split_whitespace().nth(19)?.parse().ok()
}

/// The seconds since the epoch.
fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default()
}

/// Builds the record of one transfer.
fn record(pending: &ReceivePending, outcome: &crate::transfer::TransferOutcome) -> TransferRecord {
    TransferRecord {
        protocol: pending.protocol,
        sender_alias: pending.sender.alias.clone(),
        sender_fingerprint: pending.sender.fingerprint.clone(),
        files: outcome.files.clone(),
        accepted: outcome.accepted,
        error: outcome.error.clone(),
        at: now_seconds(),
    }
}

/// Describes an event, for the state file.
fn describe(event: &AdapterEvent) -> String {
    match event {
        AdapterEvent::Discovered(peer) => {
            format!(
                "{} peer {} appeared",
                peer.protocol.display_name(),
                peer.alias
            )
        }
        AdapterEvent::Updated(peer) => {
            format!(
                "{} peer {} changed",
                peer.protocol.display_name(),
                peer.alias
            )
        }
        AdapterEvent::Lost(peer) => {
            format!(
                "{} peer {} is gone",
                peer.protocol.display_name(),
                peer.alias
            )
        }
        AdapterEvent::ReceivePending(pending) => format!(
            "{} transfer from {}",
            pending.protocol.display_name(),
            pending.sender.alias
        ),
        AdapterEvent::ReceiveExpired { protocol, .. } => format!(
            "a {} transfer expired before it was decided",
            protocol.display_name()
        ),
        AdapterEvent::Progress(_) => "a transfer changed its progress".to_owned(),
    }
}

/// Creates a connected pair of sockets.
///
/// The first one is read by the parent, the second one is inherited by the
/// child.
fn socket_pair() -> Result<(RawFd, RawFd)> {
    let mut fds = [0 as RawFd; 2];

    // SAFETY: `socketpair` writes two descriptors into the array.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) } == -1 {
        bail!(
            "could not create the readiness socket: {}",
            std::io::Error::last_os_error()
        );
    }

    Ok((fds[0], fds[1]))
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

/// A duration, as a readable string.
fn human(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;

    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}
