//! BLE module
//!
//! Provides BLE functionality compatible with CatShare (Android).
//!
//! # Modules
//!
//! - `scanner`: BLE scanner (discovers receiver devices)
//! - `client`: BLE client (connects to the receiver and exchanges P2P info)
//! - `server`: GATT server (acts as the receiver and waits for connections)
//! - `advertiser`: advertiser (publishes the receiver advertisement)
//!
//! # UUID constants
//!
//! All UUIDs match CatShare:
//! - `SERVICE_UUID`: advertisement service UUID, used for device discovery
//! - `MAIN_SERVICE_UUID`: GATT main service UUID
//! - `STATUS_CHAR_UUID`: characteristic for reading DeviceInfo
//! - `P2P_CHAR_UUID`: characteristic for writing P2pInfo

pub mod advertiser;
pub mod client;
pub mod gatt;
pub mod scanner;
pub mod security;
pub mod server;

use uuid::Uuid;

/// CatShare/MTA uses 16-bit UUID: 0x3331
///
/// Note: advertisement discovery uses the custom base 008123456789, not the standard Bluetooth base
pub const ADV_SERVICE_UUID: Uuid = Uuid::from_u128(0x00003331_0000_1000_8000_008123456789);

/// Service UUID (used for matching during scanning; uses the standard Bluetooth base)
///
/// Some devices may use the standard Bluetooth base UUID: 00003331-0000-1000-8000-00805f9b34fb
/// Scanning should check both ADV_SERVICE_UUID and SERVICE_UUID
pub const SERVICE_UUID: Uuid = Uuid::from_u128(0x00003331_0000_1000_8000_00805f9b34fb);

/// GATT Main Service UUID
///
/// CatShare: `00009955-0000-1000-8000-00805f9b34fb`
/// Used for GATT service registration; contains the STATUS and P2P characteristics
pub const MAIN_SERVICE_UUID: Uuid = Uuid::from_u128(0x00009955_0000_1000_8000_00805f9b34fb);

/// STATUS characteristic UUID (reads DeviceInfo)
///
/// CatShare: `00009954-0000-1000-8000-00805f9b34fb`
pub const STATUS_CHAR_UUID: Uuid = Uuid::from_u128(0x00009954_0000_1000_8000_00805f9b34fb);

/// P2P characteristic UUID (writes P2pInfo)
///
/// CatShare: `00009953-0000-1000-8000-00805f9b34fb`
pub const P2P_CHAR_UUID: Uuid = Uuid::from_u128(0x00009953_0000_1000_8000_00805f9b34fb);

/// DeviceInfo - fully compatible with CatShare's DeviceInfo
///
/// CatShare Kotlin definition:
/// ```kotlin
/// data class DeviceInfo(val state: Int, val key: String?, val mac: String, val catShare: Int? = null)
/// ```
///
/// # Fields
///
/// - `state`: device state (usually 0)
/// - `key`: Base64-encoded ECDH public key (SPKI format)
/// - `mac`: device MAC address
/// - `cat_share`: protocol version number (serialized as `catShare`)
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub state: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub mac: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cat_share: Option<i32>,
}

impl DeviceInfo {
    /// Create a new DeviceInfo
    ///
    /// # Arguments
    ///
    /// - `public_key`: Base64-encoded ECDH public key
    /// - `mac`: device MAC address
    pub fn new(public_key: String, mac: String) -> Self {
        Self {
            state: 0,
            key: Some(public_key),
            mac,
            cat_share: Some(1),
        }
    }
}

// Re-exports
pub use client::BleClient;
pub use scanner::{BleScanner, ChannelScanCallback, DiscoveredDevice, ScanCallback};
pub use server::{GattServer, GattServerHandle, P2pReceiveEvent};

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that the UUID constants match CatShare
    #[test]
    fn test_uuid_constants() {
        // Advertisement service UUID (CatShare ADV_SERVICE_UUID)
        assert_eq!(
            ADV_SERVICE_UUID.to_string(),
            "00003331-0000-1000-8000-008123456789"
        );

        // Service UUID with the standard Bluetooth base
        assert_eq!(
            SERVICE_UUID.to_string(),
            "00003331-0000-1000-8000-00805f9b34fb"
        );

        // GATT main service UUID
        assert_eq!(
            MAIN_SERVICE_UUID.to_string(),
            "00009955-0000-1000-8000-00805f9b34fb"
        );

        // STATUS characteristic UUID
        assert_eq!(
            STATUS_CHAR_UUID.to_string(),
            "00009954-0000-1000-8000-00805f9b34fb"
        );

        // P2P characteristic UUID
        assert_eq!(
            P2P_CHAR_UUID.to_string(),
            "00009953-0000-1000-8000-00805f9b34fb"
        );
    }

    /// Verify that DeviceInfo serialization is compatible with CatShare
    #[test]
    fn test_device_info_serialization() {
        let info = DeviceInfo::new("BASE64KEY".to_string(), "AA:BB:CC:DD:EE:FF".to_string());

        let json = serde_json::to_string(&info).unwrap();

        // Verify camelCase naming
        assert!(json.contains("\"state\":"));
        assert!(json.contains("\"key\":"));
        assert!(json.contains("\"mac\":"));
        assert!(json.contains("\"catShare\":")); // not cat_share

        // Verify values
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["state"], 0);
        assert_eq!(parsed["key"], "BASE64KEY");
        assert_eq!(parsed["mac"], "AA:BB:CC:DD:EE:FF");
        assert_eq!(parsed["catShare"], 1);
    }

    /// Verify that DeviceInfo deserialization is compatible with CatShare
    #[test]
    fn test_device_info_deserialization() {
        // Simulate JSON sent by CatShare
        let json = r#"{"state":0,"key":"ABC123","mac":"11:22:33:44:55:66","catShare":2}"#;

        let info: DeviceInfo = serde_json::from_str(json).unwrap();

        assert_eq!(info.state, 0);
        assert_eq!(info.key, Some("ABC123".to_string()));
        assert_eq!(info.mac, "11:22:33:44:55:66");
        assert_eq!(info.cat_share, Some(2));
    }

    /// Verify DeviceInfo optional field handling
    #[test]
    fn test_device_info_optional_fields() {
        // No key or catShare
        let json = r#"{"state":1,"mac":"00:00:00:00:00:00"}"#;

        let info: DeviceInfo = serde_json::from_str(json).unwrap();

        assert_eq!(info.state, 1);
        assert_eq!(info.key, None);
        assert_eq!(info.mac, "00:00:00:00:00:00");
        assert_eq!(info.cat_share, None);
    }

    /// Verify that an empty key is skipped during serialization
    #[test]
    fn test_device_info_skip_none() {
        let info = DeviceInfo {
            state: 0,
            key: None,
            mac: "AA:BB:CC:DD:EE:FF".to_string(),
            cat_share: None,
        };

        let json = serde_json::to_string(&info).unwrap();

        // None fields should be skipped
        assert!(!json.contains("key"));
        assert!(!json.contains("catShare"));
    }
}
