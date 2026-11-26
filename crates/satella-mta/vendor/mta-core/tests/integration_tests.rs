//! Integration tests - encryption and protocol compatibility
//!
//! Verifies interoperability between cattysend-core and CatShare (Android).

use satella_mta_vendor::ble::DeviceInfo;
use satella_mta_vendor::crypto::BleSecurity;
use satella_mta_vendor::wifi::P2pInfo;

/// Test the complete ECDH key exchange flow
///
/// Simulates the key agreement process between a sender and a receiver
#[test]
fn test_full_ecdh_handshake() {
    // 1. The receiver generates a key pair and broadcasts the public key
    let receiver = BleSecurity::new().unwrap();
    let receiver_pub_key = receiver.get_public_key().to_string();

    // 2. The sender generates a key pair
    let sender = BleSecurity::new().unwrap();
    let sender_pub_key = sender.get_public_key().to_string();

    // 3. The sender derives the session key with the receiver's public key and encrypts the P2pInfo
    let sender_cipher = sender.derive_session_key(&receiver_pub_key).unwrap();

    let original_ssid = "DIRECT-test1234";
    let original_psk = "password123";
    let original_mac = "AA:BB:CC:DD:EE:FF";

    let encrypted_ssid = sender_cipher.encrypt(original_ssid).unwrap();
    let encrypted_psk = sender_cipher.encrypt(original_psk).unwrap();
    let encrypted_mac = sender_cipher.encrypt(original_mac).unwrap();

    // 4. The receiver derives the session key with the sender's public key and decrypts
    let receiver_cipher = receiver.derive_session_key(&sender_pub_key).unwrap();

    let decrypted_ssid = receiver_cipher.decrypt(&encrypted_ssid).unwrap();
    let decrypted_psk = receiver_cipher.decrypt(&encrypted_psk).unwrap();
    let decrypted_mac = receiver_cipher.decrypt(&encrypted_mac).unwrap();

    // 5. Verify the decryption results
    assert_eq!(decrypted_ssid, original_ssid);
    assert_eq!(decrypted_psk, original_psk);
    assert_eq!(decrypted_mac, original_mac);
}

/// Test full compatibility of DeviceInfo with the CatShare JSON format
#[test]
fn test_device_info_catshare_compatibility() {
    // Simulate the DeviceInfo JSON sent by CatShare
    let catshare_json = r#"{
        "state": 0,
        "key": "MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE...",
        "mac": "AA:BB:CC:DD:EE:FF",
        "catShare": 123
    }"#;

    // Parse
    let device_info: DeviceInfo = serde_json::from_str(catshare_json).unwrap();

    assert_eq!(device_info.state, 0);
    assert_eq!(
        device_info.key,
        Some("MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE...".to_string())
    );
    assert_eq!(device_info.mac, "AA:BB:CC:DD:EE:FF");
    assert_eq!(device_info.cat_share, Some(123));

    // Serialize back
    let serialized = serde_json::to_string(&device_info).unwrap();

    // Verify camelCase naming
    assert!(serialized.contains("\"catShare\":123"));
    assert!(!serialized.contains("\"cat_share\""));
}

/// Test full compatibility of P2pInfo with the CatShare JSON format
#[test]
fn test_p2p_info_catshare_compatibility() {
    // Simulate the encrypted P2pInfo JSON sent by CatShare
    let catshare_json = r#"{
        "id": "a1b2",
        "ssid": "ENCRYPTED_BASE64_SSID",
        "psk": "ENCRYPTED_BASE64_PSK",
        "mac": "ENCRYPTED_BASE64_MAC",
        "port": 8443,
        "key": "SENDER_PUBLIC_KEY_BASE64",
        "catShare": 1
    }"#;

    // Parse
    let p2p_info: P2pInfo = serde_json::from_str(catshare_json).unwrap();

    assert_eq!(p2p_info.id, Some("a1b2".to_string()));
    assert_eq!(p2p_info.ssid, "ENCRYPTED_BASE64_SSID");
    assert_eq!(p2p_info.psk, "ENCRYPTED_BASE64_PSK");
    assert_eq!(p2p_info.mac, "ENCRYPTED_BASE64_MAC");
    assert_eq!(p2p_info.port, 8443);
    assert_eq!(p2p_info.key, Some("SENDER_PUBLIC_KEY_BASE64".to_string()));
    assert_eq!(p2p_info.cat_share, Some(1));

    // Serialize back
    let serialized = serde_json::to_string(&p2p_info).unwrap();

    // Verify camelCase naming
    assert!(serialized.contains("\"catShare\":1"));
    assert!(!serialized.contains("\"cat_share\""));
}

/// Test the complete P2pInfo encryption/decryption flow
#[test]
fn test_p2p_info_encryption_roundtrip() {
    // Create key pairs
    let receiver = BleSecurity::new().unwrap();
    let sender = BleSecurity::new().unwrap();

    let receiver_pub = receiver.get_public_key().to_string();
    let sender_pub = sender.get_public_key().to_string();

    // Original P2P info
    let original = P2pInfo::new(
        "DIRECT-abcd1234".to_string(),
        "secretpass".to_string(),
        "11:22:33:44:55:66".to_string(),
        9443,
    );

    // The sender encrypts
    let sender_cipher = sender.derive_session_key(&receiver_pub).unwrap();
    let encrypted = P2pInfo::with_encryption(
        "sender123".to_string(),
        sender_cipher.encrypt(&original.ssid).unwrap(),
        sender_cipher.encrypt(&original.psk).unwrap(),
        sender_cipher.encrypt(&original.mac).unwrap(),
        original.port,
        sender_pub.clone(),
    );

    // Serialize (simulating BLE transfer)
    let json = serde_json::to_string(&encrypted).unwrap();

    // Parse
    let received: P2pInfo = serde_json::from_str(&json).unwrap();

    // The receiver decrypts
    let receiver_cipher = receiver.derive_session_key(&sender_pub).unwrap();

    let decrypted_ssid = receiver_cipher.decrypt(&received.ssid).unwrap();
    let decrypted_psk = receiver_cipher.decrypt(&received.psk).unwrap();
    let decrypted_mac = receiver_cipher.decrypt(&received.mac).unwrap();

    // Verify
    assert_eq!(decrypted_ssid, original.ssid);
    assert_eq!(decrypted_psk, original.psk);
    assert_eq!(decrypted_mac, original.mac);
    assert_eq!(received.port, original.port);
}

/// Test public key format compatibility (SPKI)
#[test]
fn test_public_key_spki_format() {
    let security = BleSecurity::new().unwrap();
    let pub_key_b64 = security.get_public_key();

    // Base64 decode
    use base64::Engine;
    let pub_key_bytes = base64::engine::general_purpose::STANDARD
        .decode(pub_key_b64)
        .unwrap();

    // SPKI format verification
    // A P-256 SPKI public key should:
    // 1. Start with 0x30 (SEQUENCE)
    // 2. Be about 91 bytes long
    assert_eq!(
        pub_key_bytes[0], 0x30,
        "Public key should be SPKI format (0x30 = SEQUENCE)"
    );

    assert!(
        pub_key_bytes.len() >= 88 && pub_key_bytes.len() <= 92,
        "SPKI P-256 public key should be ~91 bytes, got {}",
        pub_key_bytes.len()
    );
}
