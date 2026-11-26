//! WiFi P2P receiver (client)
//!
//! Connects to the P2P hotspot created by the sender.
//!
//! # Connection strategy (highest priority first)
//!
//! 1. **NmClient D-Bus**: use NetworkManager's native D-Bus interface
//! 2. **Plain WiFi connection**: fall back to a simple command line (backup only)
//!
//! # Notes
//!
//! - The DHCP-assigned IP address is obtained automatically after connecting
//! - Disconnecting cleans up the related network configuration

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, info, warn};
use tokio::sync::Mutex;

use crate::wifi::P2pInfo;
use crate::wifi::nm_dbus::NmClient;

/// WiFi P2P receiver configuration
#[derive(Debug, Clone)]
pub struct P2pReceiverConfig {
    /// Main WiFi interface name (for example, wlan0)
    pub main_interface: String,
    /// P2P device interface name (for example, p2p-dev-wlan0)
    pub p2p_device: Option<String>,
    /// Whether to prefer keeping the existing WiFi connection
    pub preserve_wifi: bool,
}

impl Default for P2pReceiverConfig {
    fn default() -> Self {
        Self {
            main_interface: "wlan0".to_string(),
            p2p_device: None,
            preserve_wifi: true,
        }
    }
}

/// Active connection state
struct ActiveConnection {
    connection_name: String,
    _connection_path: Option<String>,
    used_p2p_mode: bool,
}

/// WiFi P2P receiver
pub struct WiFiP2pReceiver {
    config: P2pReceiverConfig,
    nm_client: Arc<Mutex<Option<NmClient>>>,
    active_connection: Arc<Mutex<Option<ActiveConnection>>>,
}

impl WiFiP2pReceiver {
    pub fn new(interface: &str) -> Self {
        Self {
            config: P2pReceiverConfig {
                main_interface: interface.to_string(),
                ..Default::default()
            },
            nm_client: Arc::new(Mutex::new(None)),
            active_connection: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_config(config: P2pReceiverConfig) -> Self {
        Self {
            config,
            nm_client: Arc::new(Mutex::new(None)),
            active_connection: Arc::new(Mutex::new(None)),
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

    /// Connect to the P2P hotspot
    ///
    /// Returns the assigned IP address
    pub async fn connect(&mut self, info: &P2pInfo) -> anyhow::Result<String> {
        info!(
            "Connecting to WiFi Direct: ssid='{}', preserve_wifi={}",
            info.ssid, self.config.preserve_wifi
        );

        // Try the NmClient D-Bus path
        match self.connect_nm_dbus(info).await {
            Ok(ip) => {
                info!("Connected via NetworkManager D-Bus, IP: {}", ip);
                return Ok(ip);
            }
            Err(e) => {
                warn!("NM D-Bus connection failed: {}, trying fallback", e);
            }
        }

        // Fall back to the simple nmcli command
        self.connect_nmcli_fallback(info).await
    }

    /// Connect through the NmClient D-Bus interface
    async fn connect_nm_dbus(&self, info: &P2pInfo) -> anyhow::Result<String> {
        self.ensure_nm_client().await?;

        let client_guard = self.nm_client.lock().await;
        let client = client_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("NM client not initialized"))?;

        let conn_name = format!(
            "cattysend-wifi-{}",
            &info.ssid[..std::cmp::min(8, info.ssid.len())]
        );

        // Delete any leftover connection with the same name
        let _ = client.delete_connection_by_name(&conn_name).await;

        // Trigger a WiFi scan
        if let Some(device) = client
            .find_wifi_device(Some(&self.config.main_interface))
            .await?
        {
            let _ = client.request_wifi_scan(&device).await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        // Create the connection
        let conn_path = client
            .create_wifi_connection(&info.ssid, &info.psk, Some(&self.config.main_interface))
            .await?;

        // Find the device
        let device = client
            .find_wifi_device(Some(&self.config.main_interface))
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("WiFi device {} not found", self.config.main_interface)
            })?;

        let active_conn = client
            .activate_connection(&conn_path.as_ref(), &device)
            .await?;

        // Wait for the IP address assignment
        let ip = client
            .wait_for_ip(&active_conn.as_ref(), Duration::from_secs(20))
            .await?;

        // Record the active connection
        let mut active = self.active_connection.lock().await;
        *active = Some(ActiveConnection {
            connection_name: conn_name,
            _connection_path: Some(conn_path.to_string()),
            used_p2p_mode: false,
        });

        Ok(ip)
    }

    /// Connect through the nmcli command line (fallback)
    async fn connect_nmcli_fallback(&self, info: &P2pInfo) -> anyhow::Result<String> {
        debug!("Connecting via nmcli fallback");

        // Trigger a scan
        let _ = Command::new("nmcli")
            .args([
                "device",
                "wifi",
                "rescan",
                "ifname",
                &self.config.main_interface,
            ])
            .output();

        tokio::time::sleep(Duration::from_secs(2)).await;

        // Try to connect
        let output = Command::new("nmcli")
            .args([
                "device",
                "wifi",
                "connect",
                &info.ssid,
                "password",
                &info.psk,
                "ifname",
                &self.config.main_interface,
            ])
            .output()?;

        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("nmcli connection failed: {}", err));
        }

        // Record the active connection
        let mut active = self.active_connection.lock().await;
        *active = Some(ActiveConnection {
            connection_name: info.ssid.clone(),
            _connection_path: None,
            used_p2p_mode: false,
        });

        // Wait for and read the IP address
        tokio::time::sleep(Duration::from_secs(2)).await;
        self.get_interface_ip(&self.config.main_interface)
    }

    /// Disconnect and clean up
    pub async fn disconnect(&mut self) -> anyhow::Result<()> {
        info!("Disconnecting WiFi P2P connection");

        let active = self.active_connection.lock().await.take();

        if let Some(conn) = active {
            // Try to delete through NetworkManager D-Bus
            if let Ok(()) = self.ensure_nm_client().await {
                let client_guard = self.nm_client.lock().await;
                if let Some(client) = client_guard.as_ref() {
                    let _ = client
                        .delete_connection_by_name(&conn.connection_name)
                        .await;
                }
            }

            // Also try deleting through nmcli (fallback)
            let _ = Command::new("nmcli")
                .args(["connection", "delete", &conn.connection_name])
                .output();
        }

        Ok(())
    }

    /// Get the interface's IP address
    fn get_interface_ip(&self, interface: &str) -> anyhow::Result<String> {
        let output = Command::new("ip")
            .args(["-o", "addr", "show", interface])
            .output()?;

        let stdout = String::from_utf8(output.stdout)?;
        for line in stdout.lines() {
            if line.contains("inet ") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if let Some(pos) = parts.iter().position(|&s| s == "inet")
                    && let Some(ip) = parts
                        .get(pos + 1)
                        .and_then(|ip_range| ip_range.split('/').next())
                {
                    return Ok(ip.to_string());
                }
            }
        }
        Err(anyhow::anyhow!(
            "Could not find IP address for {}",
            interface
        ))
    }

    /// Check whether a connection is active
    pub async fn is_connected(&self) -> bool {
        let active = self.active_connection.lock().await;
        active.is_some()
    }

    /// Get the name of the interface currently in use
    pub fn active_interface(&self) -> &str {
        &self.config.main_interface
    }

    /// Whether the original WiFi connection is still active
    pub async fn is_dual_connected(&self) -> bool {
        let active = self.active_connection.lock().await;
        active.as_ref().map(|a| a.used_p2p_mode).unwrap_or(false)
    }
}

impl Drop for WiFiP2pReceiver {
    fn drop(&mut self) {
        // Drop is synchronous, so the only cleanup available here is nmcli
        if let Ok(active) = self.active_connection.try_lock()
            && let Some(conn) = active.as_ref()
        {
            let _ = Command::new("nmcli")
                .args(["connection", "delete", &conn.connection_name])
                .output();
        }
    }
}
