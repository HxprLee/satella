//! The checks that the notification service works against a real bus.
//!
//! The unit tests cover the text and the bookkeeping. These cover the part
//! that only a bus can show: that the notification is really shown, that the
//! two buttons are really on it, and that the click really comes back as the
//! right decision.
//!
//! The test runs on its own private bus with a fake notification service
//! that owns the well-known name, so it does not need a desktop and does not
//! touch the session of the user.

use std::collections::HashMap;

use satella_cli::notify::{Decision, Notifier};
use zbus::zvariant::Value;

use satella_core::peer::{FileEntry, Peer, ReceivePending};
use satella_core::Protocol;

/// The fake notification service.
///
/// It answers `Notify` with an id of its own.
struct FakeNotifications {
    /// The next id the service hands out.
    next_id: std::sync::Mutex<u32>,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl FakeNotifications {
    /// Answers the notification with an id.
    ///
    /// The nine arguments are the ones the specification gives the method.
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        _replaces_id: u32,
        _app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<String>,
        _hints: HashMap<String, Value>,
        expire_timeout: i32,
    ) -> u32 {
        println!("notify: app={app_name} summary={summary:?} body={body:?}");
        println!("notify: actions={actions:?} expire_timeout={expire_timeout}");

        let mut next_id = self.next_id.lock().unwrap();
        *next_id += 1;
        *next_id
    }

    /// Closes a notification. The fake one does nothing.
    fn close_notification(&self, id: u32) {
        println!("close_notification: id={id}");
    }
}

/// Emits the click on a button, the way a notification daemon does.
///
/// The signal has to come from the connection that owns the well-known name,
/// because the client's proxy only listens for the signal of that name.
async fn click(connection: &zbus::Connection, id: u32, action_key: &str) -> zbus::Result<()> {
    connection
        .emit_signal(
            Option::<&str>::None,
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "ActionInvoked",
            &(id, action_key),
        )
        .await
}

/// Builds an incoming transfer of one file.
fn pending() -> ReceivePending {
    ReceivePending {
        protocol: Protocol::LocalSend,
        transfer_id: "transfer-1".to_owned(),
        sender: Peer::new(Protocol::LocalSend, "phone", "fingerprint-phone"),
        files: vec![FileEntry {
            name: "holiday.jpg".to_owned(),
            size: 2_400_000,
            mime: None,
            sha256: None,
        }],
        total_size: 2_400_000,
    }
}

/// Starts a bus of the test's own and points the session at it.
///
/// The test needs the well-known name of the notification service, and the
/// desktop of the user who runs the test already owns it. A bus of its own
/// keeps the test from touching the desktop.
///
/// The bus is given back with its pid, so the test can stop it when it is
/// done.
async fn private_bus() -> Option<(zbus::Connection, i32)> {
    // The daemon forks and prints its address and its pid.
    let output = std::process::Command::new("dbus-daemon")
        .args(["--session", "--fork", "--print-address", "--print-pid"])
        .output()
        .ok()?;

    let text = String::from_utf8(output.stdout).ok()?;
    let mut lines = text.lines();
    let address = lines.next()?.trim().to_owned();
    let pid: i32 = lines.next()?.trim().parse().ok()?;

    if address.is_empty() {
        return None;
    }

    std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &address);

    let connection = zbus::Connection::session().await.ok()?;
    Some((connection, pid))
}

/// Stops a bus that the test started.
fn stop_bus(pid: i32) {
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// The notification is shown on the bus, it carries both buttons, and the
/// click comes back as the right decision.
#[tokio::test]
async fn the_notification_carries_the_buttons_and_the_click_comes_back() {
    let (service_connection, bus_pid) = private_bus()
        .await
        .expect("the test needs a bus of its own");

    // The service has its own connection, because that is the one that owns
    // the well-known name and therefore the one the click comes from.
    service_connection
        .request_name("org.freedesktop.Notifications")
        .await
        .expect("the name must be free");

    let service = FakeNotifications {
        next_id: std::sync::Mutex::new(0),
    };
    service_connection
        .object_server()
        .at("/org/freedesktop/Notifications", service)
        .await
        .expect("the service must be on the bus");

    // The client connects to the same bus and shows the notification.
    let mut notifier = Notifier::connect()
        .await
        .expect("the notifier must connect");
    let id = notifier
        .ask(&pending())
        .await
        .expect("the notification must show");
    assert!(id > 0, "the service hands out an id");

    // The click on "Accept" comes back as the accept decision.
    click(&service_connection, id, "accept")
        .await
        .expect("the click must go out");

    let decision = tokio::time::timeout(std::time::Duration::from_secs(5), notifier.next_click())
        .await
        .expect("the click must come back")
        .expect("the channel must be open");

    assert_eq!(decision.0, id, "the click names the notification");
    assert_eq!(
        decision.1,
        Decision::Accept,
        "the accept button was pressed"
    );

    // The click on "Refuse" comes back as the refuse decision.
    let id = notifier
        .ask(&pending())
        .await
        .expect("the notification must show");
    click(&service_connection, id, "refuse")
        .await
        .expect("the click must go out");

    let decision = tokio::time::timeout(std::time::Duration::from_secs(5), notifier.next_click())
        .await
        .expect("the click must come back")
        .expect("the channel must be open");

    assert_eq!(
        decision.1,
        Decision::Refuse,
        "the refuse button was pressed"
    );

    stop_bus(bus_pid);
}
