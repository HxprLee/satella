//! WiFi P2P module
//!
//! Provides WiFi Direct functionality compatible with CatShare.
//!
//! # Modules
//!
//! - `nm_dbus`: NetworkManager D-Bus client (recommended)
//! - `p2p_go`: P2P group owner (sender; MTA peers only join P2P groups)
//! - `p2p_sender`: P2P hotspot creation (sender)
//! - `p2p_receiver`: P2P connection (receiver)
//!
//! # P2pInfo
//!
//! Core data structure used to exchange WiFi connection information during the
//! BLE handshake. Sensitive fields (SSID, PSK, MAC) can be encrypted with
//! AES-CTR.

pub mod nm_dbus;
pub mod p2p_go;
pub mod p2p_receiver;
pub mod p2p_sender;

#[cfg(test)]
mod tests;

pub use nm_dbus::NmClient;
pub use p2p_receiver::{P2pReceiverConfig, WiFiP2pReceiver};
pub use p2p_sender::{P2pConfig, WiFiP2pSender};

/// Check whether the process has the necessary permissions
///
/// Returns (has_nmcli, has_net_raw)
/// - has_nmcli: whether NetworkManager (nmcli) is installed on the system
/// - has_net_raw: whether the process has CAP_NET_RAW (used for BLE scanning)
pub fn check_capabilities() -> (bool, bool) {
    let mut has_nmcli = false;
    let mut has_net_raw = false;

    // Check whether we are root
    unsafe {
        if libc::geteuid() == 0 {
            return (true, true);
        }
    }

    // Check CAP_NET_RAW (used for BLE scanning)
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("CapEff:")
                && let Some(hex) = line.split_whitespace().nth(1)
                && let Ok(caps) = u64::from_str_radix(hex, 16)
            {
                // CAP_NET_RAW = 13
                has_net_raw = (caps & (1 << 13)) != 0;
            }
        }
    }

    // Check whether nmcli is available
    if let Ok(output) = std::process::Command::new("nmcli")
        .arg("--version")
        .output()
    {
        has_nmcli = output.status.success();
    }

    (has_nmcli, has_net_raw)
}

/// P2pInfo - fully compatible with CatShare's P2pInfo
///
/// CatShare Kotlin definition:
/// ```kotlin
/// data class P2pInfo(
///     val id: String?,
///     val ssid: String,
///     val psk: String,
///     val mac: String,
///     val port: Int,
///     val key: String? = null,
///     val catShare: Int? = null,
/// )
/// ```
///
/// # Fields
///
/// - `id`: sender ID (optional)
/// - `ssid`: WiFi SSID (can be encrypted)
/// - `psk`: WiFi password (can be encrypted)
/// - `mac`: sender MAC address (can be encrypted)
/// - `port`: HTTPS service port
/// - `key`: sender ECDH public key (used to decrypt the fields above)
/// - `cat_share`: protocol version number
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct P2pInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub ssid: String,
    pub psk: String,
    pub mac: String,
    pub port: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cat_share: Option<i32>,
}

impl P2pInfo {
    /// Create an unencrypted P2pInfo
    pub fn new(ssid: String, psk: String, mac: String, port: i32) -> Self {
        Self {
            id: None,
            ssid,
            psk,
            mac,
            port,
            key: None,
            cat_share: Some(1),
        }
    }

    /// Create a P2pInfo with encrypted fields
    ///
    /// # Parameters
    ///
    /// - `id`: sender ID
    /// - `ssid_encrypted`: encrypted SSID (Base64)
    /// - `psk_encrypted`: encrypted password (Base64)
    /// - `mac_encrypted`: encrypted MAC address (Base64)
    /// - `port`: service port
    /// - `sender_public_key`: sender public key (the receiver uses it to decrypt)
    pub fn with_encryption(
        id: String,
        ssid_encrypted: String,
        psk_encrypted: String,
        mac_encrypted: String,
        port: i32,
        sender_public_key: String,
    ) -> Self {
        Self {
            id: Some(id),
            ssid: ssid_encrypted,
            psk: psk_encrypted,
            mac: mac_encrypted,
            port,
            key: Some(sender_public_key),
            cat_share: Some(1),
        }
    }

    /// Get the sender's HTTPS address
    pub fn get_server_url(&self, host_ip: &str) -> String {
        format!("https://{}:{}", host_ip, self.port)
    }
}
