//! NetworkManager D-Bus client
//!
//! Talks directly to the NetworkManager daemon over D-Bus instead of spawning
//! `nmcli` subprocesses.
//!
//! # Advantages
//!
//! - **Type safety**: the Rust type system guarantees interface correctness
//! - **Async**: native async/await support
//! - **Stable**: no need to parse CLI output, avoiding compatibility issues
//!   caused by format changes
//! - **Efficient**: no subprocess forking
//!
//! # Usage
//!
//! ```ignore
//! use cattysend_core::wifi::nm_dbus::NmClient;
//!
//! let client = NmClient::new().await?;
//!
//! // Get the device list
//! let devices = client.get_wifi_devices().await?;
//!
//! // Create a hotspot
//! let conn = client.create_hotspot("DIRECT-abc", "password123", "a").await?;
//!
//! // Activate the connection
//! client.activate_connection(&conn, &device).await?;
//! ```

use std::collections::HashMap;
use std::time::Duration;

use std::ops::Deref;

use anyhow::{Context, Result};
use log::{debug, info};
use zbus::Connection;
use zbus::proxy;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

// The NetworkManager D-Bus path is handled automatically by the zbus proxy macro

/// Proxy for the NetworkManager main interface
#[proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait NetworkManager {
    /// Get all network devices
    fn get_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    /// Activate a connection
    fn activate_connection(
        &self,
        connection: &ObjectPath<'_>,
        device: &ObjectPath<'_>,
        specific_object: &ObjectPath<'_>,
    ) -> zbus::Result<OwnedObjectPath>;

    /// Deactivate a connection
    fn deactivate_connection(&self, active_connection: &ObjectPath<'_>) -> zbus::Result<()>;

    /// Add and activate a connection (simplified version)
    fn add_and_activate_connection(
        &self,
        connection: HashMap<&str, HashMap<&str, Value<'_>>>,
        device: &ObjectPath<'_>,
        specific_object: &ObjectPath<'_>,
    ) -> zbus::Result<(OwnedObjectPath, OwnedObjectPath)>;

    /// NetworkManager version
    #[zbus(property)]
    fn version(&self) -> zbus::Result<String>;

    /// Current connection state
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;

    /// List of active connections
    #[zbus(property)]
    fn active_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
}

/// Proxy for the NetworkManager.Settings interface
#[proxy(
    interface = "org.freedesktop.NetworkManager.Settings",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager/Settings"
)]
trait NmSettings {
    /// Add a new connection
    fn add_connection(
        &self,
        connection: HashMap<&str, HashMap<&str, Value<'_>>>,
    ) -> zbus::Result<OwnedObjectPath>;

    /// List all connections
    fn list_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
}

/// Proxy for the NetworkManager.Settings.Connection interface
#[proxy(
    interface = "org.freedesktop.NetworkManager.Settings.Connection",
    default_service = "org.freedesktop.NetworkManager"
)]
trait NmConnection {
    /// Get the connection settings
    fn get_settings(&self) -> zbus::Result<HashMap<String, HashMap<String, OwnedValue>>>;

    /// Delete the connection
    fn delete(&self) -> zbus::Result<()>;
}

/// Proxy for the NetworkManager.Device interface
#[proxy(
    interface = "org.freedesktop.NetworkManager.Device",
    default_service = "org.freedesktop.NetworkManager"
)]
trait NmDevice {
    /// Device interface name (e.g. wlan0)
    #[zbus(property)]
    fn interface(&self) -> zbus::Result<String>;

    /// Device type (2=WiFi, 30=WiFi-P2P)
    #[zbus(property)]
    fn device_type(&self) -> zbus::Result<u32>;

    /// Device state
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;

    /// Whether the device is managed
    #[zbus(property)]
    fn managed(&self) -> zbus::Result<bool>;

    /// Current active connection
    #[zbus(property)]
    fn active_connection(&self) -> zbus::Result<OwnedObjectPath>;

    /// Hardware address (MAC)
    #[zbus(property)]
    fn hw_address(&self) -> zbus::Result<String>;

    /// Disconnect the device
    fn disconnect(&self) -> zbus::Result<()>;
}

/// Proxy for the NetworkManager.Device.Wireless interface
#[proxy(
    interface = "org.freedesktop.NetworkManager.Device.Wireless",
    default_service = "org.freedesktop.NetworkManager"
)]
trait NmDeviceWireless {
    /// Trigger a WiFi scan
    fn request_scan(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;

    /// Get all access points
    fn get_all_access_points(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
}

/// Proxy for the NetworkManager.Connection.Active interface
#[proxy(
    interface = "org.freedesktop.NetworkManager.Connection.Active",
    default_service = "org.freedesktop.NetworkManager"
)]
trait NmActiveConnection {
    /// Connection ID
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;

    /// Connection state
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;

    /// IP4 config object path
    #[zbus(property)]
    fn ip4_config(&self) -> zbus::Result<OwnedObjectPath>;

    /// Path of the saved connection profile
    ///
    /// This is a profile under `/org/freedesktop/NetworkManager/Settings/...`,
    /// which is exactly the path `activate_connection()` expects.
    #[zbus(property)]
    fn connection(&self) -> zbus::Result<OwnedObjectPath>;
}

/// Proxy for the NetworkManager.IP4Config interface
#[proxy(
    interface = "org.freedesktop.NetworkManager.IP4Config",
    default_service = "org.freedesktop.NetworkManager"
)]
trait NmIp4Config {
    /// Address data (new format)
    #[zbus(property)]
    fn address_data(&self) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
}

// ============================================================================
// High-level wrappers
// ============================================================================

/// Device type constants
pub mod device_type {
    pub const WIFI: u32 = 2;
    pub const WIFI_P2P: u32 = 30;
}

/// Device state constants
pub mod device_state {
    pub const DISCONNECTED: u32 = 30;
    pub const ACTIVATED: u32 = 100;
}

/// Active connection state constants
pub mod active_connection_state {
    pub const UNKNOWN: u32 = 0;
    pub const ACTIVATING: u32 = 1;
    pub const ACTIVATED: u32 = 2;
    pub const DEACTIVATING: u32 = 3;
    pub const DEACTIVATED: u32 = 4;

    pub fn name(state: u32) -> &'static str {
        match state {
            UNKNOWN => "UNKNOWN",
            ACTIVATING => "ACTIVATING",
            ACTIVATED => "ACTIVATED",
            DEACTIVATING => "DEACTIVATING",
            DEACTIVATED => "DEACTIVATED",
            _ => "INVALID",
        }
    }
}

/// WiFi device information
#[derive(Debug, Clone)]
pub struct WifiDevice {
    /// D-Bus object path
    pub path: OwnedObjectPath,
    /// Interface name (e.g. wlan0)
    pub interface: String,
    /// Device type
    pub device_type: u32,
    /// MAC address
    pub hw_address: String,
    /// Whether the device is active
    pub is_active: bool,
}

/// NetworkManager D-Bus client
pub struct NmClient {
    connection: Connection,
}

impl NmClient {
    /// Create a new NM D-Bus client
    pub async fn new() -> Result<Self> {
        let connection = Connection::system()
            .await
            .context("Failed to connect to system D-Bus")?;

        // Verify that NetworkManager is available
        let nm = NetworkManagerProxy::new(&connection).await?;
        let version = nm.version().await?;
        info!("Connected to NetworkManager {}", version);

        Ok(Self { connection })
    }

    /// Get the NetworkManager version
    pub async fn version(&self) -> Result<String> {
        let nm = NetworkManagerProxy::new(&self.connection).await?;
        nm.version().await.context("Failed to get NM version")
    }

    /// Get all WiFi devices
    pub async fn get_wifi_devices(&self) -> Result<Vec<WifiDevice>> {
        let nm = NetworkManagerProxy::new(&self.connection).await?;
        let device_paths = nm.get_devices().await?;

        let mut wifi_devices = Vec::new();

        for path in device_paths {
            let device = NmDeviceProxy::builder(&self.connection)
                .path(&path)?
                .build()
                .await?;

            let dev_type = device.device_type().await.unwrap_or(0);

            // Only collect WiFi and WiFi-P2P devices
            if dev_type == device_type::WIFI || dev_type == device_type::WIFI_P2P {
                let interface = device.interface().await.unwrap_or_default();
                let hw_address = device.hw_address().await.unwrap_or_default();
                let state = device.state().await.unwrap_or(0);

                wifi_devices.push(WifiDevice {
                    path,
                    interface,
                    device_type: dev_type,
                    hw_address,
                    is_active: state == device_state::ACTIVATED,
                });
            }
        }

        Ok(wifi_devices)
    }

    /// Find the P2P device
    pub async fn find_p2p_device(&self) -> Result<Option<WifiDevice>> {
        let devices = self.get_wifi_devices().await?;
        Ok(devices
            .into_iter()
            .find(|d| d.device_type == device_type::WIFI_P2P))
    }

    /// Find the main WiFi device
    pub async fn find_wifi_device(&self, interface: Option<&str>) -> Result<Option<WifiDevice>> {
        let devices = self.get_wifi_devices().await?;

        if let Some(iface) = interface {
            Ok(devices.into_iter().find(|d| d.interface == iface))
        } else {
            Ok(devices
                .into_iter()
                .find(|d| d.device_type == device_type::WIFI))
        }
    }

    /// The WiFi connection profile currently active on the interface.
    ///
    /// Returns `(profile path, profile name)`, or `None` when the interface has
    /// no active connection.
    ///
    /// Remember this before creating a P2P group: NM's `Device.disconnect()`
    /// blocks the device's autoconnect permanently (reason 39), so after the
    /// group is torn down it will not reconnect on its own; the old profile has
    /// to be reactivated explicitly.
    pub async fn active_wifi_profile(
        &self,
        interface: &str,
    ) -> Result<Option<(OwnedObjectPath, String)>> {
        let Some(device) = self.find_wifi_device(Some(interface)).await? else {
            return Ok(None);
        };

        let device_proxy = NmDeviceProxy::builder(&self.connection)
            .path(&device.path)?
            .build()
            .await?;

        let active = device_proxy.active_connection().await?;
        if active.as_str() == "/" {
            return Ok(None);
        }

        let active_proxy = NmActiveConnectionProxy::builder(&self.connection)
            .path(&active)?
            .build()
            .await?;

        let profile = active_proxy.connection().await?;
        if profile.as_str() == "/" {
            return Ok(None);
        }

        let id = active_proxy.id().await.unwrap_or_default();
        Ok(Some((profile, id)))
    }

    /// Create a WiFi hotspot connection profile
    pub async fn create_hotspot(
        &self,
        ssid: &str,
        password: &str,
        band: &str,
        interface: &str,
    ) -> Result<OwnedObjectPath> {
        let settings = NmSettingsProxy::new(&self.connection).await?;

        // Build the connection profile
        let connection_settings = self.build_hotspot_settings(ssid, password, band, interface);

        let conn_path = settings
            .add_connection(connection_settings)
            .await
            .map_err(|err| anyhow::anyhow!("Failed to create hotspot connection: {err}"))?;

        info!("Created hotspot connection: {:?}", conn_path);
        Ok(conn_path)
    }

    /// Build the hotspot connection settings
    fn build_hotspot_settings<'a>(
        &self,
        ssid: &'a str,
        password: &'a str,
        band: &'a str,
        interface: &'a str,
    ) -> HashMap<&'a str, HashMap<&'a str, Value<'a>>> {
        let mut settings: HashMap<&str, HashMap<&str, Value>> = HashMap::new();

        // connection section
        let mut connection: HashMap<&str, Value> = HashMap::new();
        let conn_id = format!(
            "cattysend-hotspot-{}",
            &ssid[..std::cmp::min(8, ssid.len())]
        );
        connection.insert("id", Value::Str(conn_id.into()));
        connection.insert("type", Value::Str("802-11-wireless".into()));
        connection.insert("autoconnect", Value::Bool(false));
        connection.insert("interface-name", Value::Str(interface.into()));
        settings.insert("connection", connection);

        // 802-11-wireless section
        let mut wireless: HashMap<&str, Value> = HashMap::new();
        wireless.insert("ssid", Value::Array(ssid.as_bytes().into()));
        wireless.insert("mode", Value::Str("ap".into()));
        wireless.insert("band", Value::Str(band.into()));
        settings.insert("802-11-wireless", wireless);

        // 802-11-wireless-security section
        let mut wireless_security: HashMap<&str, Value> = HashMap::new();
        wireless_security.insert("key-mgmt", Value::Str("wpa-psk".into()));
        wireless_security.insert("psk", Value::Str(password.into()));
        settings.insert("802-11-wireless-security", wireless_security);

        // ipv4 section (shared mode - automatic DHCP)
        //
        // MTA P2P groups use 192.168.49.0/24: the receiver uses the gateway of
        // its own address (x.x.x.1) to find the sender, and NetworkManager's
        // default 10.42.0.1 would send it to the wrong place.
        //
        // NetworkManager's `addresses` property is `aau`: each address is a
        // sequence of uint32 values. The first is the address packed into a
        // little-endian 32-bit integer (192.168.49.1 is 0x0131A8C0), the
        // second is the prefix length, and the third is the gateway flag.
        let packed = 192u32 | (168 << 8) | (49 << 16) | (1 << 24);
        let mut address = zbus::zvariant::Array::new(
            zbus::zvariant::Signature::from_static_str("u").expect("valid signature"),
        );
        let _ = address.append(Value::U32(packed));
        let _ = address.append(Value::U32(24));
        let _ = address.append(Value::U32(0));
        let mut addresses = zbus::zvariant::Array::new(
            zbus::zvariant::Signature::from_static_str("au").expect("valid signature"),
        );
        let _ = addresses.append(Value::Array(address));

        let mut ipv4: HashMap<&str, Value> = HashMap::new();
        ipv4.insert("method", Value::Str("shared".into()));
        ipv4.insert("addresses", Value::Array(addresses));
        settings.insert("ipv4", ipv4);

        // ipv6 section
        let mut ipv6: HashMap<&str, Value> = HashMap::new();
        ipv6.insert("method", Value::Str("ignore".into()));
        settings.insert("ipv6", ipv6);

        settings
    }

    /// Create a WiFi client connection profile
    pub async fn create_wifi_connection(
        &self,
        ssid: &str,
        password: &str,
        interface: Option<&str>,
    ) -> Result<OwnedObjectPath> {
        let settings = NmSettingsProxy::new(&self.connection).await?;

        let connection_settings = self.build_wifi_client_settings(ssid, password, interface);

        let conn_path = settings
            .add_connection(connection_settings)
            .await
            .context("Failed to create WiFi connection")?;

        info!("Created WiFi connection: {:?}", conn_path);
        Ok(conn_path)
    }

    /// Build the WiFi client connection settings
    fn build_wifi_client_settings<'a>(
        &self,
        ssid: &'a str,
        password: &'a str,
        interface: Option<&'a str>,
    ) -> HashMap<&'a str, HashMap<&'a str, Value<'a>>> {
        let mut settings: HashMap<&str, HashMap<&str, Value>> = HashMap::new();

        // connection section
        let mut connection: HashMap<&str, Value> = HashMap::new();
        let conn_id = format!("cattysend-wifi-{}", &ssid[..std::cmp::min(8, ssid.len())]);
        connection.insert("id", Value::Str(conn_id.into()));
        connection.insert("type", Value::Str("802-11-wireless".into()));
        connection.insert("autoconnect", Value::Bool(false));
        if let Some(iface) = interface {
            connection.insert("interface-name", Value::Str(iface.into()));
        }
        settings.insert("connection", connection);

        // 802-11-wireless section
        let mut wireless: HashMap<&str, Value> = HashMap::new();
        wireless.insert("ssid", Value::Array(ssid.as_bytes().into()));
        wireless.insert("mode", Value::Str("infrastructure".into()));
        settings.insert("802-11-wireless", wireless);

        // 802-11-wireless-security section
        let mut wireless_security: HashMap<&str, Value> = HashMap::new();
        wireless_security.insert("key-mgmt", Value::Str("wpa-psk".into()));
        wireless_security.insert("psk", Value::Str(password.into()));
        settings.insert("802-11-wireless-security", wireless_security);

        // ipv4 section
        let mut ipv4: HashMap<&str, Value> = HashMap::new();
        ipv4.insert("method", Value::Str("auto".into()));
        settings.insert("ipv4", ipv4);

        // ipv6 section
        let mut ipv6: HashMap<&str, Value> = HashMap::new();
        ipv6.insert("method", Value::Str("auto".into()));
        settings.insert("ipv6", ipv6);

        settings
    }

    /// Activate a connection
    pub async fn activate_connection(
        &self,
        connection_path: &ObjectPath<'_>,
        device: &WifiDevice,
    ) -> Result<OwnedObjectPath> {
        let nm = NetworkManagerProxy::new(&self.connection).await?;

        let device_path = device.path.as_ref();
        let active_conn = nm
            .activate_connection(
                connection_path,
                &device_path,
                &ObjectPath::from_static_str_unchecked("/"),
            )
            .await
            .context("Failed to activate connection")?;

        info!("Activated connection: {:?}", active_conn);
        Ok(active_conn)
    }

    /// Deactivate a connection
    pub async fn deactivate_connection(&self, active_connection: &ObjectPath<'_>) -> Result<()> {
        let nm = NetworkManagerProxy::new(&self.connection).await?;
        nm.deactivate_connection(active_connection).await?;
        Ok(())
    }

    /// Delete a connection profile
    pub async fn delete_connection(&self, connection_path: &ObjectPath<'_>) -> Result<()> {
        let conn = NmConnectionProxy::builder(&self.connection)
            .path(connection_path)?
            .build()
            .await?;

        conn.delete().await.context("Failed to delete connection")?;
        debug!("Deleted connection: {:?}", connection_path);
        Ok(())
    }

    /// Delete a connection by name
    pub async fn delete_connection_by_name(&self, name: &str) -> Result<bool> {
        let settings = NmSettingsProxy::new(&self.connection).await?;
        let connections = settings.list_connections().await?;

        for conn_path in connections {
            let conn = NmConnectionProxy::builder(&self.connection)
                .path(&conn_path)?
                .build()
                .await?;

            if let Ok(conn_settings) = conn.get_settings().await
                && let Some(connection_section) = conn_settings.get("connection")
                && let Some(id_value) = connection_section.get("id")
                && let Value::Str(id_str) = id_value.deref()
                && id_str.as_str() == name
            {
                conn.delete().await?;
                debug!("Deleted connection by name: {}", name);
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Trigger a WiFi scan
    pub async fn request_wifi_scan(&self, device: &WifiDevice) -> Result<()> {
        let wireless = NmDeviceWirelessProxy::builder(&self.connection)
            .path(&device.path)?
            .build()
            .await?;

        wireless
            .request_scan(HashMap::new())
            .await
            .context("Failed to request WiFi scan")?;

        Ok(())
    }

    /// Wait for the connection to activate (without waiting for IP configuration, for hotspot mode)
    pub async fn wait_for_activation(
        &self,
        active_connection: &ObjectPath<'_>,
        timeout: Duration,
    ) -> Result<()> {
        let start = std::time::Instant::now();
        let mut last_state = 0u32;

        loop {
            if start.elapsed() > timeout {
                return Err(anyhow::anyhow!(
                    "Timeout waiting for connection activation (last state: {})",
                    active_connection_state::name(last_state)
                ));
            }

            let active = NmActiveConnectionProxy::builder(&self.connection)
                .path(active_connection)?
                .build()
                .await?;

            let state = active.state().await.unwrap_or(0);

            // Log state changes
            if state != last_state {
                debug!(
                    "Connection state changed: {} -> {}",
                    active_connection_state::name(last_state),
                    active_connection_state::name(state)
                );
                last_state = state;
            }

            match state {
                active_connection_state::ACTIVATED => {
                    info!("Connection activated successfully");
                    return Ok(());
                }
                active_connection_state::DEACTIVATED | active_connection_state::DEACTIVATING => {
                    return Err(anyhow::anyhow!(
                        "Connection failed to activate (state: {})",
                        active_connection_state::name(state)
                    ));
                }
                _ => {
                    // UNKNOWN, ACTIVATING - keep waiting
                }
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Wait for the connection to activate and obtain an IP address
    pub async fn wait_for_ip(
        &self,
        active_connection: &ObjectPath<'_>,
        timeout: Duration,
    ) -> Result<String> {
        let start = std::time::Instant::now();

        loop {
            if start.elapsed() > timeout {
                return Err(anyhow::anyhow!("Timeout waiting for IP address"));
            }

            let active = NmActiveConnectionProxy::builder(&self.connection)
                .path(active_connection)?
                .build()
                .await?;

            let state = active.state().await.unwrap_or(0);

            if state == active_connection_state::ACTIVATED {
                // Get the IPv4 configuration
                if let Ok(ip4_path) = active.ip4_config().await
                    && ip4_path.as_str() != "/"
                {
                    let ip4 = NmIp4ConfigProxy::builder(&self.connection)
                        .path(&ip4_path)?
                        .build()
                        .await?;

                    if let Ok(addresses) = ip4.address_data().await {
                        for addr in addresses {
                            if let Some(address_value) = addr.get("address")
                                && let Value::Str(ip_str) = address_value.deref()
                            {
                                return Ok(ip_str.to_string());
                            }
                        }
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Disconnect the device
    pub async fn disconnect_device(&self, device: &WifiDevice) -> Result<()> {
        let dev = NmDeviceProxy::builder(&self.connection)
            .path(&device.path)?
            .build()
            .await?;

        dev.disconnect()
            .await
            .context("Failed to disconnect device")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: these tests require the system D-Bus and a running NetworkManager
    // They may need to be skipped in CI environments

    #[tokio::test]
    #[ignore = "requires system D-Bus and NetworkManager"]
    async fn test_nm_client_version() {
        let client = NmClient::new().await.unwrap();
        let version = client.version().await.unwrap();
        assert!(!version.is_empty());
        println!("NetworkManager version: {}", version);
    }

    #[tokio::test]
    #[ignore = "requires system D-Bus and NetworkManager"]
    async fn test_get_wifi_devices() {
        let client = NmClient::new().await.unwrap();
        let devices = client.get_wifi_devices().await.unwrap();

        for device in &devices {
            println!(
                "Device: {} (type={}, mac={})",
                device.interface, device.device_type, device.hw_address
            );
        }
    }
}
