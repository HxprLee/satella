//! The desktop notification service.
//!
//! The daemon asks the user through a desktop notification: an incoming
//! transfer shows up with an "Accept" and a "Refuse" button, and the click
//! decides what happens to the transfer.
//!
//! The notification is the FreeDesktop Desktop Notifications Specification,
//! spoken over the session bus. `notify-rust` would pull a second D-Bus stack
//! in for one call and one signal, so the proxy is written out here instead;
//! the zbus version is the one the vendored MTA engine already uses.
//!
//! The service does not time anything out. The sender waits for the decision
//! and gives up on its own terms, so a notification that is left alone dies
//! with the transfer it was about.

use std::collections::HashMap;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use satella_core::peer::ReceivePending;
use tokio::sync::mpsc;
use zbus::zvariant::Value;

/// The name the daemon announces itself with on the bus.
const APP_NAME: &str = "satella-cli";

/// The key of the "Accept" button.
const ACCEPT: &str = "accept";

/// The key of the "Refuse" button.
const REFUSE: &str = "refuse";

/// The notification bus, as the specification describes it.
///
/// The `Notify` method has nine arguments because the specification says so.
/// It is the shape of the protocol, not a choice.
#[allow(clippy::too_many_arguments)]
#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    /// Shows a notification and returns the id it was given.
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<&str>,
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    /// Takes a notification off the screen.
    fn close_notification(&self, id: u32) -> zbus::Result<()>;

    /// The user pressed one of the notification's buttons.
    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: &str) -> zbus::Result<()>;

    /// The notification went away, either because the user dismissed it or
    /// because the notification daemon closed it.
    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

/// What the user chose in the notification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
    /// The user clicked "Accept".
    Accept,
    /// The user clicked "Refuse".
    Refuse,
}

/// The notification service.
///
/// It owns the bus connection and reads the clicks off it. The clicks arrive
/// on a channel, so the daemon's event loop never waits on the bus itself.
pub struct Notifier {
    proxy: NotificationsProxy<'static>,
    clicks: mpsc::Receiver<(u32, Decision)>,
}

impl Notifier {
    /// Opens the session bus and starts listening for the clicks.
    ///
    /// It fails when there is no session bus, which is how the daemon learns
    /// that it cannot ask and has to take the transfer instead.
    pub async fn connect() -> Result<Self> {
        let connection = zbus::Connection::session()
            .await
            .context("could not reach the notification bus")?;
        let proxy = NotificationsProxy::new(&connection)
            .await
            .context("could not reach the notification service")?;

        let (clicks_tx, clicks) = mpsc::channel(16);

        // The stream is registered here and not in the task below: the
        // registration and the lookup of the notification service's owner
        // take a round trip on the bus, and a click that arrives in between
        // would be missed. The task only reads what is already registered.
        let invoked = proxy
            .receive_action_invoked()
            .await
            .context("could not listen for the notification actions")?;

        tokio::spawn(async move {
            listen_for_clicks(invoked, clicks_tx).await;
        });

        Ok(Self { proxy, clicks })
    }

    /// Shows the notification for an incoming transfer.
    ///
    /// The returned id identifies the notification, so the click can be tied
    /// back to the transfer it was about.
    pub async fn ask(&self, pending: &ReceivePending) -> Result<u32> {
        let mut hints = HashMap::new();
        hints.insert("category", Value::from("satella.incoming-transfer"));
        hints.insert("urgency", Value::from(1u8));

        let summary = format!(
            "Incoming transfer from {}",
            if pending.sender.alias.is_empty() {
                pending.protocol.display_name().to_owned()
            } else {
                pending.sender.alias.clone()
            }
        );
        let body = body_of(pending);

        self.proxy
            .notify(
                APP_NAME,
                0,
                "",
                &summary,
                &body,
                vec![ACCEPT, "Accept", REFUSE, "Refuse"],
                hints,
                // The notification lives until the transfer is decided or
                // dies; the sender's own timeout is the deadline.
                -1,
            )
            .await
            .context("could not show the notification")
    }

    /// Waits for the next click.
    ///
    /// The id is the id of the notification that was clicked, so the daemon
    /// knows which transfer the click belongs to.
    pub async fn next_click(&mut self) -> Option<(u32, Decision)> {
        self.clicks.recv().await
    }

    /// Takes a notification off the screen.
    ///
    /// It is used when the transfer died on its own: the buttons of a
    /// notification about a transfer that no longer exists would do nothing.
    pub async fn close(&self, id: u32) {
        if let Err(err) = self.proxy.close_notification(id).await {
            tracing::debug!("could not close the notification {id}: {err}");
        }
    }
}

/// Reads the clicks off the bus and puts them on the channel.
///
/// The stream is already registered when this is called, so a click that
/// arrives right after the notification is shown is not missed.
async fn listen_for_clicks(
    mut invoked: ActionInvokedStream<'static>,
    clicks: mpsc::Sender<(u32, Decision)>,
) {
    while let Some(signal) = invoked.next().await {
        let Ok(args) = signal.args() else {
            continue;
        };

        let decision = match *args.action_key() {
            ACCEPT => Decision::Accept,
            REFUSE => Decision::Refuse,
            // A key the service does not know is not a decision. The
            // specification lets a notification carry a "default" key, which
            // means the notification was clicked rather than one of its
            // buttons.
            other => {
                tracing::debug!("the notification was activated with {other:?}");
                continue;
            }
        };

        if clicks.send((*args.id(), decision)).await.is_err() {
            // The daemon is gone; nothing is left to tell.
            return;
        }
    }
}

/// The body of the notification: what is being offered.
fn body_of(pending: &ReceivePending) -> String {
    let mut body = String::new();

    for file in &pending.files {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&file.name);
        body.push_str(" (");
        body.push_str(&human_size(file.size));
        body.push(')');
    }

    if pending.files.len() > 1 {
        body.push_str(&format!("\n{} in total", human_size(pending.total_size)));
    }

    if body.is_empty() {
        body.push_str("The sender offers no file name");
    }

    body
}

/// A size, as a readable string.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];

    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use satella_core::peer::{FileEntry, Peer};
    use satella_core::Protocol;

    /// A pending transfer of one file.
    fn pending_of(names: &[(&str, u64)]) -> ReceivePending {
        let files = names
            .iter()
            .map(|(name, size)| FileEntry {
                name: (*name).to_owned(),
                size: *size,
                mime: None,
                sha256: None,
            })
            .collect();

        ReceivePending {
            protocol: Protocol::LocalSend,
            transfer_id: "transfer-1".to_owned(),
            sender: Peer::new(Protocol::LocalSend, "phone", "fingerprint"),
            files,
            total_size: names.iter().map(|(_, size)| size).sum(),
        }
    }

    /// The body names the file, so the user knows what is on the way.
    #[test]
    fn the_body_names_the_files() {
        let body = body_of(&pending_of(&[("photo.jpg", 2_400_000)]));

        assert!(body.contains("photo.jpg"), "{body}");
        assert!(body.contains("2.3 MB"), "{body}");
    }

    /// Several files are listed, and the total is given as well.
    #[test]
    fn the_body_lists_every_file_and_the_total() {
        let body = body_of(&pending_of(&[("a.txt", 10), ("b.txt", 20)]));

        assert!(body.contains("a.txt"), "{body}");
        assert!(body.contains("b.txt"), "{body}");
        assert!(body.contains("30 B"), "{body}");
    }

    /// A transfer without a file name still says something.
    #[test]
    fn the_body_of_an_empty_transfer_says_so() {
        let body = body_of(&pending_of(&[]));

        assert!(!body.is_empty());
    }

    /// A size is shown in the unit that fits it.
    #[test]
    fn a_size_is_shown_in_a_readable_unit() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }
}
