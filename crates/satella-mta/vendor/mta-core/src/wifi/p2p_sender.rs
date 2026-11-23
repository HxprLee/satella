//! WiFi P2P 发送端 (Group Owner / Hotspot)
//!
//! 互传联盟的接收端只加入真正的 WiFi P2P 组：它按 BLE 里拿到的 SSID 做
//! `p2p_connect`，不会去连普通热点。所以发送端必须先用 `wpa_supplicant`
//! 建一个组所有者，并自己配地址、跑 DHCP。
//!
//! # 实现方式
//!
//! 1. 优先用 `WpaP2pGroupOwner` 创建真正的 P2P 组（需要 root）
//! 2. 失败时退回 `NmClient` 普通热点：两个 Satella 设备之间仍然可用
//!
//! # 注意事项
//!
//! - P2P 组所有者要 root（wpa_supplicant 的 D-Bus 接口只允许 root）
//! - 5GHz 频段优先，建组失败再退 2.4GHz

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, info, warn};
use tokio::sync::Mutex;

use crate::wifi::P2pInfo;
use crate::wifi::nm_dbus::NmClient;
use crate::wifi::p2p_go::{
    DhcpServer, P2pGroup, WpaP2pGroupOwner, configure_go_interface, is_root,
    remove_go_interface_address,
};

/// WiFi P2P 配置
pub struct P2pConfig {
    /// 网络接口名称 (通常是 wlan0)
    pub interface: String,
    /// SSID 前缀
    pub ssid_prefix: String,
    /// 是否使用 5GHz
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

/// 活动连接信息（用于清理）
enum ActiveHotspot {
    /// NetworkManager 创建的普通热点。
    NetworkManager {
        connection_name: String,
        _connection_path: Option<String>,
    },
    /// wpa_supplicant 创建的真正 P2P 组。
    GroupOwner {
        owner: WpaP2pGroupOwner,
        group: P2pGroup,
        dhcp: DhcpServer,
    },
}

pub struct WiFiP2pSender {
    config: P2pConfig,
    nm_client: Arc<Mutex<Option<NmClient>>>,
    active_hotspot: Arc<Mutex<Option<ActiveHotspot>>>,
}

impl WiFiP2pSender {
    pub fn new(interface: &str) -> Self {
        Self {
            config: P2pConfig {
                interface: interface.to_string(),
                ..Default::default()
            },
            nm_client: Arc::new(Mutex::new(None)),
            active_hotspot: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_config(config: P2pConfig) -> Self {
        Self {
            config,
            nm_client: Arc::new(Mutex::new(None)),
            active_hotspot: Arc::new(Mutex::new(None)),
        }
    }

    /// 初始化 NM 客户端
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

    /// 生成随机 SSID 和 PSK
    ///
    /// SSID 必须是 `DIRECT-xx` 形式（`xx` 为两位十六进制）。互传联盟的
    /// 客户端只扫描这个格式的热点，长一点的随机后缀会被忽略。
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

    /// 创建 WiFi P2P 组（组所有者模式）
    ///
    /// 返回 P2P 信息，包含 SSID、密码和端口。
    pub async fn create_group(&self, port: i32) -> anyhow::Result<P2pInfo> {
        // 获取 MAC 地址
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
                        "Run satella with sudo or pkexec to let Android receivers join; \
                         falling back to a plain hotspot that only Satella peers can use"
                    );
                }
            }
        }

        let (ssid, psk) = self.generate_credentials();

        // 尝试使用 NmClient (D-Bus) 创建热点
        match self.create_hotspot_nm(&ssid, &psk).await {
            Ok(_) => {
                info!("Hotspot created via NetworkManager D-Bus");
            }
            Err(e) => {
                // 有些网卡不支持 5GHz AP 模式：热点会一直停在 ACTIVATING。
                // 这种情况退回 2.4GHz 再试一次，而不是直接失败。
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

    /// 用 `wpa_supplicant` 建一个 P2P 组，配好地址并启动 DHCP。
    async fn create_group_owner(&self) -> anyhow::Result<(String, String)> {
        // NetworkManager 还占着接口的话，它稍后激活别的连接会把组拆掉。
        self.disconnect_from_nm().await;

        let owner = WpaP2pGroupOwner::new(&self.config.interface).await?;

        let primary = if self.config.use_5ghz { 5745 } else { 2412 };
        let group = match owner.create_group(primary, Duration::from_secs(20)).await {
            Ok(group) => group,
            Err(err) => {
                if !self.config.use_5ghz {
                    return Err(err);
                }

                warn!("P2P group on {primary} MHz failed: {err:#}, retrying on 2412 MHz");
                owner.create_group(2412, Duration::from_secs(20)).await?
            }
        };

        if group.ssid.is_empty() || group.psk.is_empty() {
            let _ = owner.remove_group().await;
            return Err(anyhow::anyhow!(
                "the P2P group has an empty SSID or passphrase"
            ));
        }

        if let Err(err) = configure_go_interface(&group.interface).await {
            let _ = remove_go_interface_address(&group.interface).await;
            let _ = owner.remove_group().await;
            return Err(err);
        }

        let dhcp = match DhcpServer::start(&group.interface).await {
            Ok(dhcp) => dhcp,
            Err(err) => {
                let _ = remove_go_interface_address(&group.interface).await;
                let _ = owner.remove_group().await;
                return Err(err);
            }
        };

        let ssid = group.ssid.clone();
        let psk = group.psk.clone();

        *self.active_hotspot.lock().await = Some(ActiveHotspot::GroupOwner { owner, group, dhcp });

        Ok((ssid, psk))
    }

    /// 让 NetworkManager 松开接口。
    ///
    /// 不松开的话，组刚建好 NM 就可能把 `wlan0` 抢回去激活原来的 Wi-Fi，
    /// P2P 组会被直接拆掉。
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

        // 等 NM 真正把接口放下来，再去建组。
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    /// 使用 NetworkManager D-Bus 创建热点
    async fn create_hotspot_nm(&self, ssid: &str, psk: &str) -> anyhow::Result<()> {
        let band = if self.config.use_5ghz { "a" } else { "bg" };
        self.create_hotspot_nm_with_band(ssid, psk, band).await
    }

    /// 使用 NetworkManager D-Bus 在指定频段创建热点
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

        // 先删除可能存在的旧连接
        let conn_name = format!(
            "cattysend-hotspot-{}",
            &ssid[..std::cmp::min(8, ssid.len())]
        );
        let _ = client.delete_connection_by_name(&conn_name).await;

        // 创建热点连接配置
        let conn_path = client
            .create_hotspot(ssid, psk, band, &self.config.interface)
            .await?;

        // 查找设备
        let device = client
            .find_wifi_device(Some(&self.config.interface))
            .await?
            .ok_or_else(|| anyhow::anyhow!("WiFi device {} not found", self.config.interface))?;

        // 激活连接
        let active_conn = client
            .activate_connection(&conn_path.as_ref(), &device)
            .await?;

        // 等待连接状态变为 ACTIVATED。地址是 nm_dbus 里配的
        // 192.168.49.1/24，shared 模式会在这上面起 DHCP。
        client
            .wait_for_activation(&active_conn.as_ref(), Duration::from_secs(15))
            .await?;
        info!("Hotspot activated successfully");

        // 记录活动热点信息（用于清理）
        let mut hotspot = self.active_hotspot.lock().await;
        *hotspot = Some(ActiveHotspot::NetworkManager {
            connection_name: conn_name,
            _connection_path: Some(conn_path.to_string()),
        });

        Ok(())
    }

    /// 停止 P2P 组
    pub async fn stop_group(&self) -> anyhow::Result<()> {
        debug!("Stopping P2P group/hotspot");

        let hotspot = self.active_hotspot.lock().await.take();

        match hotspot {
            Some(ActiveHotspot::NetworkManager {
                connection_name, ..
            }) => {
                // 使用 NM D-Bus 删除连接
                if let Ok(()) = self.ensure_nm_client().await {
                    let client_guard = self.nm_client.lock().await;
                    if let Some(client) = client_guard.as_ref() {
                        let _ = client.delete_connection_by_name(&connection_name).await;
                    }
                }
            }
            Some(ActiveHotspot::GroupOwner { owner, group, dhcp }) => {
                let _ = dhcp.stop().await;

                let mut first_error = None;

                if let Err(err) = owner.remove_group().await {
                    first_error = Some(err);
                }

                if let Err(err) = remove_go_interface_address(&group.interface).await
                    && first_error.is_none()
                {
                    first_error = Some(err);
                }

                if let Some(err) = first_error {
                    return Err(err);
                }
            }
            None => {}
        }

        Ok(())
    }

    /// 获取接口 MAC 地址
    ///
    /// 互传联盟的规范要求发送端提供 P2P 接口的 MAC（接收端拿它和 Wi-Fi
    /// 组的组所有者地址比对），所以先查 P2P 接口。
    fn get_mac_address(&self) -> anyhow::Result<String> {
        // 尝试读取 p2p 接口
        let p2p_path = format!("/sys/class/net/p2p-dev-{}/address", self.config.interface);
        if let Ok(mac) = std::fs::read_to_string(&p2p_path) {
            return Ok(mac.trim().to_uppercase());
        }

        // 退回普通接口
        let path = format!("/sys/class/net/{}/address", self.config.interface);
        if let Ok(mac) = std::fs::read_to_string(&path) {
            return Ok(mac.trim().to_uppercase());
        }

        // 返回默认 MAC
        Ok("02:00:00:00:00:00".to_string())
    }

    /// 获取热点的 IP 地址
    pub fn get_hotspot_ip(&self) -> anyhow::Result<String> {
        // 两种热点都用 192.168.49.1：这是互传联盟约定的网关地址。
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

        // 返回默认 IP
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

        // 互传联盟的客户端只扫描 `DIRECT-xx` 形式的热点。
        assert!(ssid.starts_with("DIRECT-"));
        assert_eq!(ssid.len(), 9); // "DIRECT-" (7) + 2 hex chars
        assert!(ssid[7..].chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(psk.len(), 8);
    }
}
