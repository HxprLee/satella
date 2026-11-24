//! Diagnostic probe for the Wi-Fi Direct group owner path.
//!
//! It is ignored by default because it needs root and a real Wi-Fi adapter.
//! Run it with:
//!
//! ```text
//! pkexec ./target/debug/deps/p2p_go_probe-<hash> --ignored --nocapture
//! ```

use std::time::Duration;

use satella_mta_vendor::ble::DiscoveredDevice;
use satella_mta_vendor::wifi::p2p_go::{
    DhcpServer, WpaP2pGroupOwner, configure_go_interface, remove_go_interface_address,
};
use satella_mta_vendor::wifi::{NmClient, P2pConfig, WiFiP2pSender};
use satella_mta_vendor::workflow::{SendOptions, Sender, SimpleSendCallback};

#[tokio::test]
#[ignore = "requires root and a real Wi-Fi adapter"]
async fn p2p_go_starts_and_serves_dhcp() {
    // NetworkManager must let go of wlan0 first, exactly like the sender does.
    if let Ok(client) = NmClient::new().await {
        if let Ok(Some(device)) = client.find_wifi_device(Some("wlan0")).await {
            let _ = client.disconnect_device(&device).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    let owner = WpaP2pGroupOwner::new("wlan0").await.expect("P2P owner");
    let group = owner
        .create_group(2412, Duration::from_secs(20))
        .await
        .expect("P2P group");

    println!(
        "GROUP ssid={:?} psk={:?} iface={:?} freq={}",
        group.ssid, group.psk, group.interface, group.frequency
    );
    assert!(group.ssid.starts_with("DIRECT-"));
    assert!(!group.psk.is_empty());
    assert_eq!(group.frequency, 2412);

    configure_go_interface(&group.interface)
        .await
        .expect("configure interface");

    let address_output = std::process::Command::new("ip")
        .args(["addr", "show", &group.interface])
        .output()
        .expect("ip addr");
    let address_output = String::from_utf8_lossy(&address_output.stdout);
    assert!(
        address_output.contains("192.168.49.1/24"),
        "missing GO address:\n{address_output}"
    );

    let dhcp = DhcpServer::start(&group.interface)
        .await
        .expect("DHCP server");

    let pid_file = format!("/run/satella-mta/dnsmasq-{}.pid", group.interface);
    let pid = std::fs::read_to_string(&pid_file).expect("dnsmasq pid file");
    assert!(!pid.trim().is_empty());

    println!("GROUP READY; keeping it up for 20 seconds");
    tokio::time::sleep(Duration::from_secs(20)).await;

    dhcp.stop().await.expect("stop DHCP");
    remove_go_interface_address(&group.interface)
        .await
        .expect("remove address");
    owner.remove_group().await.expect("remove group");

    assert!(
        !std::path::Path::new(&pid_file).exists()
            || std::fs::read_to_string(&pid_file)
                .map(|pid| pid.trim().is_empty())
                .unwrap_or(true),
        "dnsmasq pid file was not cleaned up"
    );
}

#[tokio::test]
#[ignore = "requires root and a real Wi-Fi adapter"]
async fn sender_create_group_integration() {
    let sender = WiFiP2pSender::with_config(P2pConfig {
        interface: "wlan0".to_string(),
        ssid_prefix: "DIRECT-".to_string(),
        use_5ghz: false,
    });

    let info = sender.create_group(8443).await.expect("sender group");
    println!(
        "SENDER ssid={:?} psk={:?} mac={:?} port={}",
        info.ssid, info.psk, info.mac, info.port
    );

    assert!(info.ssid.starts_with("DIRECT-"));
    assert!(!info.psk.is_empty());
    assert_eq!(info.port, 8443);

    let interface = std::process::Command::new("iw")
        .args(["dev", "wlan0", "info"])
        .output()
        .expect("iw dev");
    let interface = String::from_utf8_lossy(&interface.stdout);
    assert!(
        interface.contains("type P2P-GO"),
        "wlan0 is not a P2P GO:\n{interface}"
    );

    let addresses = std::process::Command::new("ip")
        .args(["addr", "show", "wlan0"])
        .output()
        .expect("ip addr");
    let addresses = String::from_utf8_lossy(&addresses.stdout);
    assert!(
        addresses.contains("192.168.49.1/24"),
        "missing GO address:\n{addresses}"
    );

    tokio::time::sleep(Duration::from_secs(5)).await;
    sender.stop_group().await.expect("stop sender group");
}

#[tokio::test]
#[ignore = "requires root and a real Wi-Fi adapter"]
async fn sender_cleans_up_when_ble_fails() {
    let sender = Sender::new(SendOptions {
        wifi_interface: "wlan0".to_string(),
        use_5ghz: false,
        sender_name: "Satella cleanup probe".to_string(),
        identity_path: None,
    })
    .expect("sender");

    // A peer that does not exist: the group is created first, then the BLE
    // connect fails. The group and DHCP must still be torn down.
    let device = DiscoveredDevice {
        name: "ghost".to_string(),
        address: "00:00:00:00:00:00".to_string(),
        sender_id: String::new(),
        brand: String::new(),
        brand_id: None,
        rssi: None,
        supports_5ghz: false,
    };

    let files = vec![std::path::PathBuf::from(
        "/usr/share/icons/hicolor/48x48/apps/ark.png",
    )];
    let (callback, _events) = SimpleSendCallback::new();
    let result = sender.send_to_device(&device, files, &callback).await;
    assert!(result.is_err(), "a ghost peer must fail");

    assert!(
        !std::path::Path::new("/run/satella-mta/dnsmasq-wlan0.pid").exists(),
        "dnsmasq pid file survived a failed send"
    );

    let addresses = std::process::Command::new("ip")
        .args(["addr", "show", "wlan0"])
        .output()
        .expect("ip addr");
    let addresses = String::from_utf8_lossy(&addresses.stdout);
    assert!(
        !addresses.contains("192.168.49.1/24"),
        "GO address survived a failed send:\n{addresses}"
    );

    let interface = std::process::Command::new("iw")
        .args(["dev", "wlan0", "info"])
        .output()
        .expect("iw dev");
    let interface = String::from_utf8_lossy(&interface.stdout);
    assert!(
        !interface.contains("type P2P-GO"),
        "P2P group survived a failed send:\n{interface}"
    );
}

/// The Wi-Fi connection that was active before the send must be back after
/// the group is torn down.
///
/// NM's `Device.disconnect()` is a user-requested disconnect: it sets the
/// device's autoconnect block, so NM never reconnects on its own. The
/// sender therefore has to remember the profile and re-activate it.
#[tokio::test]
#[ignore = "requires root and a real Wi-Fi adapter"]
async fn sender_restores_wifi_after_the_group_is_torn_down() {
    let client = NmClient::new().await.expect("NM client");

    let Some((_, profile_id)) = client.active_wifi_profile("wlan0").await.expect("profile")
    else {
        panic!("wlan0 has no active WiFi connection; connect to one first");
    };
    println!("BEFORE profile={profile_id:?}");

    let sender = WiFiP2pSender::with_config(P2pConfig {
        interface: "wlan0".to_string(),
        ssid_prefix: "DIRECT-".to_string(),
        use_5ghz: false,
    });

    sender.create_group(8443).await.expect("sender group");
    sender.stop_group().await.expect("stop sender group");

    // Give NetworkManager a moment to finish the activation.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let restored = client
        .active_wifi_profile("wlan0")
        .await
        .expect("restored profile")
        .map(|(_, id)| id);

    println!("AFTER profile={restored:?}");
    assert_eq!(
        restored.as_deref(),
        Some(profile_id.as_str()),
        "the WiFi connection was not restored after the P2P group was torn down"
    );
}
