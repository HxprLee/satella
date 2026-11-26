//! WiFi P2P sender (group owner / hotspot)
//!
//! An MTA receiver only joins a real WiFi P2P group: it uses the SSID received
//! over BLE to run `p2p_connect` and does not join an ordinary hotspot. The
//! sender must therefore create a group owner with `wpa_supplicant`, configure
//! its address, and run DHCP itself.
//!
//! # Implementation
//!
//! 1. When running as root, use [`InProcessGroupOwner`] to create a real P2P
//!    group directly.
//! 2. When running as an ordinary user, use [`HelperGroupOwner`] to call
//!    `satella-mta-helper`; only the root-only operations (create the group,
//!    configure the address, start DHCP) are handed to the helper.
//! 3. If both fail, fall back to a plain `NmClient` hotspot: it still works
//!    between two Satella devices.
//!
//! # Notes
//!
//! - The P2P group owner requires root (wpa_supplicant's D-Bus interface is
//!   root-only).
//! - Prefer the 5 GHz band and fall back to 2.4 GHz if group creation fails.
//! - The helper exits after each operation; the group and dnsmasq must stay in
//!   the system.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, info, warn};
use tokio::sync::Mutex;
use zbus::zvariant::OwnedObjectPath;

use crate::wifi::P2pInfo;
use crate::wifi::nm_dbus::NmClient;
use crate::wifi::p2p_go::{
    GroupOwnerBackend, HelperGroupOwner, InProcessGroupOwner, P2pGroup, is_root,
};

/// WiFi P2P configuration
pub struct P2pConfig {
    /// Network interface name (usually wlan0)
    pub interface: String,
    /// SSID prefix
    pub ssid_prefix: String,
    /// Whether to use 5 GHz
    pub use_5ghz: bool,
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            interface: "wlan0".to_string(),
            ssid_prefix: "DIRECT-".to_string(),
            use_5ghz: true,
        }
    }
}

/// Active connection information (used for cleanup)
enum ActiveHotspot {
    /// A plain hotspot created by NetworkManager.
    NetworkManager {
        connection_name: String,
        _connection_path: Option<String>,
    },
    /// A real P2P group created by wpa_supplicant.
    GroupOwner {
        backend: Box<dyn GroupOwnerBackend>,
        group: P2pGroup,
    },
}

pub struct WiFiP2pSender {
    config: P2pConfig,
    nm_client: Arc<Mutex<Option<NmClient>>>,
    active_hotspot: Arc<Mutex<Option<ActiveHotspot>>>,
    /// WiFi profile that was active on the interface before the group was
    /// created; it has to be reconnected after the group is torn down.
    previous_wifi_profile: Arc<Mutex<Option<(OwnedObjectPath, String)>>>,
}

impl WiFiP2pSender {
    pub fn new(interface: &str) -> Self {
        Self {
            config: P2pConfig {
                interface: interface.to_owned(),
                ..Default::default()
            },
            nm_client: Arc::new(Mutex::new(None)),
            active_hotspot: Arc::new(Mutex::new(None)),
            previous_wifi_profile: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_config(config: P2pConfig) -> Self {
        Self {
            config,
            nm_client: Arc::new(Mutex::new(None)),
            active_hotspot: Arc::new(Mutex::new(None)),
            previous_wifi_profile: Arc::new(Mutex::new(None)),
        }
    }

    /// Initialize the NetworkManager client
    async fn ensure_nm_client(&self) -> anyhow::Result<()> {
        let mut client = self.nm_client.lock().await;
        if client.is_none() {
            match NmClient::new().await {
                Ok(c) => {
                    info!("NetworkManager D-Bus client initialized");
                    *client = Some(c);
                }
                Err(e) => {
                    warn!("Failed to initialize NM client: {}", e);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Generate a random SSID and PSK
    ///
    /// The SSID must have the form `DIRECT-xx` (`xx` is two hexadecimal
    /// digits). MTA clients only scan for this format; longer random suffixes
    /// are ignored.
    fn generate_credentials(&self) -> (String, String) {
        let random_bytes: u8 = rand::random();

        let ssid = format!("{}{:02x}", self.config.ssid_prefix, random_bytes);
        let psk: String = (0..8)
            .map(|_| {
                let idx = rand::random::<usize>() % 36;
                if idx < 10 {
                    (b'0' + idx as u8) as char
                } else {
                    (b'a' + (idx - 10) as u8) as char
                }
            })
            .collect();

        (ssid, psk)
    }

    /// Create a WiFi P2P group (group owner mode)
    ///
    /// Returns the P2P information, including the SSID, passphrase, and port.
    pub async fn create_group(&self, port: i32) -> anyhow::Result<P2pInfo> {
        // Get the MAC address
        let mac = self.get_mac_address()?;

        match self.create_group_owner().await {
            Ok((ssid, psk)) => {
                info!("P2P group owner created: {ssid}");
                return Ok(P2pInfo::new(ssid, psk, mac, port));
            }
            Err(err) => {
                warn!("Could not create a Wi-Fi Direct group owner: {err:#}");
                if is_root() {
                    warn!(
                        "Falling back to a plain NetworkManager hotspot; Android receivers cannot join it"
                    );
                } else {
                    warn!(
                        "Falling back to a plain hotspot that only Satella peers can use; \
                         install satella-mta-helper to let Android receivers join"
                    );
                }
            }
        }

        let (ssid, psk) = self.generate_credentials();

        // Try to create the hotspot through NmClient (D-Bus)
        match self.create_hotspot_nm(&ssid, &psk).await {
            Ok(_) => {
                info!("Hotspot created via NetworkManager D-Bus");
            }
            Err(e) => {
                // Some adapters do not support 5 GHz AP mode: the hotspot stays
                // in ACTIVATING forever. Retry on 2.4 GHz instead of failing
                // immediately.
                if self.config.use_5ghz {
                    warn!("NM D-Bus hotspot failed: {}, retrying on 2.4GHz", e);
                    match self.create_hotspot_nm_with_band(&ssid, &psk, "bg").await {
                        Ok(_) => {
                            info!("Hotspot created on 2.4GHz");
                        }
                        Err(bg_err) => {
                            return Err(anyhow::anyhow!(
                                "Failed to create hotspot: NM={}, 2.4GHz={}",
                                e,
                                bg_err
                            ));
                        }
                    }
                } else {
                    return Err(anyhow::anyhow!("Failed to create hotspot: NM={}", e));
                }
            }
        }

        Ok(P2pInfo::new(ssid, psk, mac, port))
    }

    /// Create a P2P group with the available backend, configure its address,
    /// and start DHCP.
    async fn create_group_owner(&self) -> anyhow::Result<(String, String)> {
        // If NetworkManager still owns the interface, it can later activate
        // another connection and tear the group down.
        self.disconnect_from_nm().await;

        let backend = self.group_owner_backend().await?;

        let primary = if self.config.use_5ghz { 5745 } else { 2412 };
        let group = match backend.create_group(primary).await {
            Ok(group) => group,
            Err(err) => {
                if !self.config.use_5ghz {
                    return Err(err);
                }

                warn!("P2P group on {primary} MHz failed: {err:#}, retrying on 2412 MHz");
                backend.create_group(2412).await?
            }
        };

        let ssid = group.ssid.clone();
        let psk = group.psk.clone();

        *self.active_hotspot.lock().await = Some(ActiveHotspot::GroupOwner { backend, group });

        Ok((ssid, psk))
    }

    /// Choose the backend used to create the group owner.
    ///
    /// When already root, stay in-process so the hardware probes behave the
    /// same; an ordinary user goes through the helper, which keeps BLE, TLS,
    /// and the identity files in user space.
    async fn group_owner_backend(&self) -> anyhow::Result<Box<dyn GroupOwnerBackend>> {
        if is_root() {
            return Ok(Box::new(
                InProcessGroupOwner::new(&self.config.interface).await?,
            ));
        }

        match HelperGroupOwner::new(&self.config.interface) {
            Ok(backend) => Ok(Box::new(backend)),
            Err(err) => Err(anyhow::anyhow!(
                "{err}; run Satella as root if the helper is not installed"
            )),
        }
    }

    /// Make NetworkManager release the interface.
    ///
    /// Otherwise NetworkManager can grab `wlan0` back to activate the original
    /// WiFi as soon as the group is up, tearing the P2P group down.
    ///
    /// Remember which connection was active on the interface before
    /// disconnecting: NetworkManager's `Device.disconnect()` is a
    /// "user-requested disconnect" that permanently blocks autoconnect for the
    /// device, so it will not reconnect by itself after the group is torn down.
    async fn disconnect_from_nm(&self) {
        if let Err(err) = self.ensure_nm_client().await {
            debug!("NetworkManager is not available, leaving the interface alone: {err}");
            return;
        }

        let client_guard = self.nm_client.lock().await;
        let Some(client) = client_guard.as_ref() else {
            return;
        };

        match client.find_wifi_device(Some(&self.config.interface)).await {
            Ok(Some(device)) => {
                // Ask before disconnecting: once disconnected, it is too late.
                match client.active_wifi_profile(&self.config.interface).await {
                    Ok(Some((profile, id))) => {
                        info!("Remembering the active WiFi profile {id:?} to restore it later");
                        *self.previous_wifi_profile.lock().await = Some((profile, id));
                    }
                    Ok(None) => {
                        debug!(
                            "{} had no active WiFi connection; nothing to restore later",
                            self.config.interface
                        );
                    }
                    Err(err) => {
                        warn!("Could not read the active WiFi profile: {err}");
                    }
                }

                if let Err(err) = client.disconnect_device(&device).await {
                    warn!(
                        "Could not disconnect NetworkManager from {}: {err}",
                        self.config.interface
                    );
                } else {
                    debug!("Disconnected NetworkManager from {}", self.config.interface);
                }
            }
            Ok(None) => debug!("NetworkManager does not manage {}", self.config.interface),
            Err(err) => warn!("Could not look up the WiFi device: {err}"),
        }

        drop(client_guard);

        // Wait until NetworkManager has really released the interface before
        // creating the group.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    /// Reconnect the WiFi connection that was active before the group.
    ///
    /// NetworkManager does not reconnect automatically after
    /// `Device.disconnect()`, so the original profile must be activated
    /// explicitly. If the interface had no connection before sending, do
    /// nothing: never connect the user to a network they did not choose.
    async fn restore_wifi_connection(&self) -> anyhow::Result<()> {
        let Some((profile, id)) = self.previous_wifi_profile.lock().await.take() else {
            debug!("No WiFi profile to restore");
            return Ok(());
        };

        if let Err(err) = self.ensure_nm_client().await {
            return Err(err);
        }

        let client_guard = self.nm_client.lock().await;
        let Some(client) = client_guard.as_ref() else {
            return Ok(());
        };

        let Some(device) = client
            .find_wifi_device(Some(&self.config.interface))
            .await?
        else {
            warn!(
                "NetworkManager does not manage {} any more",
                self.config.interface
            );
            return Ok(());
        };

        let active = client.activate_connection(&profile, &device).await?;
        client
            .wait_for_activation(&active.as_ref(), Duration::from_secs(15))
            .await?;

        info!("Restored the WiFi connection {id:?}");
        Ok(())
    }

    /// Create a hotspot through NetworkManager D-Bus
    async fn create_hotspot_nm(&self, ssid: &str, psk: &str) -> anyhow::Result<()> {
        let band = if self.config.use_5ghz { "a" } else { "bg" };
        self.create_hotspot_nm_with_band(ssid, psk, band).await
    }

    /// Create a hotspot on the given band through NetworkManager D-Bus
    async fn create_hotspot_nm_with_band(
        &self,
        ssid: &str,
        psk: &str,
        band: &str,
    ) -> anyhow::Result<()> {
        self.ensure_nm_client().await?;

        let client_guard = self.nm_client.lock().await;
        let client = client_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("NM client not initialized"))?;

        // Delete any leftover connection with the same name first
        let conn_name = format!(
            "cattysend-hotspot-{}",
            &ssid[..std::cmp::min(8, ssid.len())]
        );
        let _ = client.delete_connection_by_name(&conn_name).await;

        // Create the hotspot connection profile
        let conn_path = client
            .create_hotspot(ssid, psk, band, &self.config.interface)
            .await?;

        // Find the device
        let device = client
            .find_wifi_device(Some(&self.config.interface))
            .await?
            .ok_or_else(|| anyhow::anyhow!("WiFi device {} not found", self.config.interface))?;

        // Activate the connection
        let active_conn = client
            .activate_connection(&conn_path.as_ref(), &device)
            .await?;

        // Wait for the connection state to become ACTIVATED. The address is
        // configured in nm_dbus as 192.168.49.1/24, and shared mode starts
        // DHCP on it.
        client
            .wait_for_activation(&active_conn.as_ref(), Duration::from_secs(15))
            .await?;
        info!("Hotspot activated successfully");

        // Record the active hotspot information (for cleanup)
        let mut hotspot = self.active_hotspot.lock().await;
        *hotspot = Some(ActiveHotspot::NetworkManager {
            connection_name: conn_name,
            _connection_path: Some(conn_path.to_string()),
        });

        Ok(())
    }

    /// Stop the P2P group
    pub async fn stop_group(&self) -> anyhow::Result<()> {
        debug!("Stopping P2P group/hotspot");

        let hotspot = self.active_hotspot.lock().await.take();

        let mut teardown_error = None;
        match hotspot {
            Some(ActiveHotspot::NetworkManager {
                connection_name, ..
            }) => {
                // Delete the connection through NetworkManager D-Bus
                if let Ok(()) = self.ensure_nm_client().await {
                    let client_guard = self.nm_client.lock().await;
                    if let Some(client) = client_guard.as_ref() {
                        let _ = client.delete_connection_by_name(&connection_name).await;
                    }
                }
            }
            Some(ActiveHotspot::GroupOwner { backend, group }) => {
                debug!("Destroying the P2P group on {}", group.interface);
                if let Err(err) = backend.destroy_group().await {
                    teardown_error = Some(err);
                }
            }
            None => {}
        }

        // Reconnect WiFi only after the group is gone: while the group still
        // exists, NetworkManager competes for the interface and tears it down.
        // A restore failure is only a warning: the transfer has already
        // succeeded, so it must not turn into an error.
        if let Err(err) = self.restore_wifi_connection().await {
            warn!("Could not restore the previous WiFi connection: {err:#}");
        }

        match teardown_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// Get the interface's MAC address
    ///
    /// The MTA specification requires the sender to provide the MAC address of
    /// the P2P interface (the receiver compares it with the group owner address
    /// of the Wi-Fi group), so check the P2P interface first.
    fn get_mac_address(&self) -> anyhow::Result<String> {
        // Try to read the p2p interface
        let p2p_path = format!("/sys/class/net/p2p-dev-{}/address", self.config.interface);
        if let Ok(mac) = std::fs::read_to_string(&p2p_path) {
            return Ok(mac.trim().to_uppercase());
        }

        // Fall back to the ordinary interface
        let path = format!("/sys/class/net/{}/address", self.config.interface);
        if let Ok(mac) = std::fs::read_to_string(&path) {
            return Ok(mac.trim().to_uppercase());
        }

        // Return the default MAC
        Ok("02:00:00:00:00:00".to_string())
    }

    /// Get the hotspot's IP address
    pub fn get_hotspot_ip(&self) -> anyhow::Result<String> {
        // Both hotspot types use 192.168.49.1: this is the gateway address
        // agreed by MTA.
        let output = Command::new("ip").args(["-o", "addr", "show"]).output()?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.contains(&self.config.interface) && line.contains("inet ") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if let Some(pos) = parts.iter().position(|&s| s == "inet")
                    && let Some(ip) = parts
                        .get(pos + 1)
                        .and_then(|ip_cidr| ip_cidr.split('/').next())
                {
                    return Ok(ip.to_string());
                }
            }
        }

        // Return the default IP
        Ok("10.42.0.1".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_credentials() {
        let sender = WiFiP2pSender::new("wlan0");
        let (ssid, psk) = sender.generate_credentials();

        // MTA clients only scan for hotspots in the `DIRECT-xx` format.
        assert!(ssid.starts_with("DIRECT-"));
        assert_eq!(ssid.len(), 9); // "DIRECT-" (7) + 2 hex chars
        assert!(ssid[7..].chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(psk.len(), 8);
    }
}
