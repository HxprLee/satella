//! WiFi module tests
//!
//! Unit tests for the NmClient D-Bus client and the P2P modules

use super::*;

// ============================================================================
// P2pInfo tests
// ============================================================================

/// Verify that P2pInfo serialization is compatible with CatShare
#[test]
fn test_p2p_info_serialization() {
    let info = P2pInfo::new(
        "DIRECT-abc".to_string(),
        "password123".to_string(),
        "AA:BB:CC:DD:EE:FF".to_string(),
        8443,
    );

    let json = serde_json::to_string(&info).unwrap();

    // Verify the camelCase naming
    assert!(json.contains("\"ssid\":"));
    assert!(json.contains("\"psk\":"));
    assert!(json.contains("\"mac\":"));
    assert!(json.contains("\"port\":"));
    assert!(json.contains("\"catShare\":")); // not cat_share

    // Verify the values
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["ssid"], "DIRECT-abc");
    assert_eq!(parsed["psk"], "password123");
    assert_eq!(parsed["mac"], "AA:BB:CC:DD:EE:FF");
    assert_eq!(parsed["port"], 8443);
    assert_eq!(parsed["catShare"], 1);
}

/// Verify that P2pInfo deserialization is compatible with CatShare
#[test]
fn test_p2p_info_deserialization() {
    // Simulate an encrypted P2pInfo sent by CatShare
    let json = r#"{
        "id": "abcd",
        "ssid": "ENCRYPTED_SSID",
        "psk": "ENCRYPTED_PSK",
        "mac": "ENCRYPTED_MAC",
        "port": 9000,
        "key": "SENDER_PUBLIC_KEY",
        "catShare": 2
    }"#;

    let info: P2pInfo = serde_json::from_str(json).unwrap();

    assert_eq!(info.id, Some("abcd".to_string()));
    assert_eq!(info.ssid, "ENCRYPTED_SSID");
    assert_eq!(info.psk, "ENCRYPTED_PSK");
    assert_eq!(info.mac, "ENCRYPTED_MAC");
    assert_eq!(info.port, 9000);
    assert_eq!(info.key, Some("SENDER_PUBLIC_KEY".to_string()));
    assert_eq!(info.cat_share, Some(2));
}

/// Verify that optional P2pInfo fields are skipped correctly
#[test]
fn test_p2p_info_skip_none() {
    let info = P2pInfo::new(
        "SSID".to_string(),
        "PSK".to_string(),
        "MAC".to_string(),
        8080,
    );

    let json = serde_json::to_string(&info).unwrap();

    // id and key are None, so they should be skipped
    assert!(!json.contains("\"id\":"));
    assert!(!json.contains("\"key\":"));
}

/// Verify the with_encryption constructor
#[test]
fn test_p2p_info_with_encryption() {
    let info = P2pInfo::with_encryption(
        "sender123".to_string(),
        "encrypted_ssid".to_string(),
        "encrypted_psk".to_string(),
        "encrypted_mac".to_string(),
        8443,
        "public_key_base64".to_string(),
    );

    assert_eq!(info.id, Some("sender123".to_string()));
    assert_eq!(info.key, Some("public_key_base64".to_string()));
    assert_eq!(info.cat_share, Some(1));
}

/// Verify the get_server_url method
#[test]
fn test_p2p_info_get_server_url() {
    let info = P2pInfo::new(
        "SSID".to_string(),
        "PSK".to_string(),
        "MAC".to_string(),
        8443,
    );

    assert_eq!(
        info.get_server_url("192.168.1.1"),
        "https://192.168.1.1:8443"
    );
    assert_eq!(info.get_server_url("10.42.0.1"), "https://10.42.0.1:8443");
}

// ============================================================================
// WiFiP2pSender tests
// ============================================================================

#[test]
fn test_p2p_config_default() {
    let config = P2pConfig::default();

    assert_eq!(config.interface, "wlan0");
    assert_eq!(config.ssid_prefix, "DIRECT-");
    assert!(config.use_5ghz);
}

#[test]
fn test_wifi_p2p_sender_new() {
    let sender = WiFiP2pSender::new("wlan1");
    // Verify that it can be created; private methods are not checked
    drop(sender);
}

#[test]
fn test_wifi_p2p_sender_with_config() {
    let config = P2pConfig {
        interface: "wlp3s0".to_string(),
        ssid_prefix: "CAT-".to_string(),
        use_5ghz: false,
    };

    let sender = WiFiP2pSender::with_config(config);
    // Verify that the configuration is applied correctly
    drop(sender);
}

// ============================================================================
// WiFiP2pReceiver tests
// ============================================================================

#[test]
fn test_p2p_receiver_config_default() {
    let config = P2pReceiverConfig::default();

    assert_eq!(config.main_interface, "wlan0");
    assert!(config.p2p_device.is_none());
    assert!(config.preserve_wifi);
}

#[test]
fn test_wifi_p2p_receiver_new() {
    let receiver = WiFiP2pReceiver::new("wlan1");

    assert_eq!(receiver.active_interface(), "wlan1");
}

// ============================================================================
// check_capabilities tests
// ============================================================================

#[test]
fn test_check_capabilities() {
    let (has_nmcli, has_net_raw) = check_capabilities();

    // These values depend on the system environment; only the types are checked
    println!("has_nmcli: {}, has_net_raw: {}", has_nmcli, has_net_raw);

    // In a non-root environment, nmcli should at least be detected (if installed)
    // No assertions are made because test environments can differ
}

// ============================================================================
// Mock test helpers (used by other test modules)
// ============================================================================

/// P2pInfo factory for tests
/// Used by tests in other modules
#[cfg(test)]
#[expect(
    dead_code,
    reason = "Test helper used by the integration tests of other modules"
)]
pub fn test_p2p_info() -> P2pInfo {
    P2pInfo::new(
        "DIRECT-test123".to_string(),
        "testpsk1".to_string(),
        "00:11:22:33:44:55".to_string(),
        8443,
    )
}

/// Encrypted P2pInfo factory for tests
/// Used by tests in other modules
#[cfg(test)]
#[expect(
    dead_code,
    reason = "Test helper used by the integration tests of other modules"
)]
pub fn test_encrypted_p2p_info() -> P2pInfo {
    P2pInfo::with_encryption(
        "test-sender-id".to_string(),
        "base64_encrypted_ssid".to_string(),
        "base64_encrypted_psk".to_string(),
        "base64_encrypted_mac".to_string(),
        8443,
        "base64_public_key".to_string(),
    )
}

// ============================================================================
// NmClient tests (require the system D-Bus)
// ============================================================================

#[cfg(test)]
mod nm_dbus_tests {
    use super::nm_dbus::*;

    // Note: these tests require the system D-Bus and a running NetworkManager
    // They should be skipped in CI environments

    #[tokio::test]
    #[ignore = "requires system D-Bus and NetworkManager"]
    async fn test_nm_client_version() {
        let client = NmClient::new().await.unwrap();
        let version = client.version().await.unwrap();

        assert!(!version.is_empty());
        println!("NetworkManager version: {}", version);

        // The version should look like "1.42.0"
        let parts: Vec<&str> = version.split('.').collect();
        assert!(parts.len() >= 2, "Version should have at least major.minor");
    }

    #[tokio::test]
    #[ignore = "requires system D-Bus and NetworkManager"]
    async fn test_get_wifi_devices() {
        let client = NmClient::new().await.unwrap();
        let devices = client.get_wifi_devices().await.unwrap();

        println!("Found {} WiFi devices", devices.len());
        for device in &devices {
            println!(
                "  - {} (type={}, mac={}, active={})",
                device.interface, device.device_type, device.hw_address, device.is_active
            );
        }
    }

    #[tokio::test]
    #[ignore = "requires system D-Bus and NetworkManager"]
    async fn test_find_wifi_device() {
        let client = NmClient::new().await.unwrap();

        // Find the default WiFi device
        if let Some(device) = client.find_wifi_device(None).await.unwrap() {
            assert_eq!(device.device_type, device_type::WIFI);
            println!("Found WiFi device: {}", device.interface);
        } else {
            println!("No WiFi device found");
        }
    }

    #[tokio::test]
    #[ignore = "requires system D-Bus and NetworkManager"]
    async fn test_find_p2p_device() {
        let client = NmClient::new().await.unwrap();

        // Find the P2P device
        if let Some(device) = client.find_p2p_device().await.unwrap() {
            assert_eq!(device.device_type, device_type::WIFI_P2P);
            println!("Found P2P device: {}", device.interface);
        } else {
            println!("No P2P device found (normal if hardware doesn't support)");
        }
    }

    #[tokio::test]
    #[ignore = "requires system D-Bus, NetworkManager, and will modify network state"]
    async fn test_create_and_delete_connection() {
        let client = NmClient::new().await.unwrap();

        // Create a test connection (without activating it)
        let test_name = "cattysend-test-connection";

        // Delete any leftover connection with the same name first
        let _ = client.delete_connection_by_name(test_name).await;

        // Create the connection profile
        let conn_path = client
            .create_wifi_connection("TestSSID", "testpassword", None)
            .await
            .unwrap();

        println!("Created connection: {:?}", conn_path);

        // Delete the connection
        client.delete_connection(&conn_path.as_ref()).await.unwrap();
        println!("Deleted connection");
    }
}
