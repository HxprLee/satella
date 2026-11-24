//! WiFi P2P 组所有者 (Group Owner)
//!
//! 互传联盟的发送端必须是一个真正的 WiFi P2P 组所有者：对端不会连普通的
//! 接入点，它只做 `p2p_connect`，加入名为 `DIRECT-xx` 的 P2P 组。
//!
//! # 为什么不用 NetworkManager
//!
//! NetworkManager 只能创建普通的接入点（AP），没有创建 P2P 组所有者的
//! 接口。所以这里直接和 `wpa_supplicant` 说话。
//!
//! # 为什么用 D-Bus 而不是 wpa_cli
//!
//! `wpa_supplicant` 的控制套接字 `/run/wpa_supplicant` 只有 root 能访问，
//! 而它的 D-Bus 接口 `fi.w1.wpa_supplicant1` 有同样的限制。所以两条路都
//! 需要提权；D-Bus 的好处是能直接拿到组的信息（SSID、密码），不用去猜。
//!
//! # NoGroupIface
//!
//! 默认情况下 `wpa_supplicant` 会为 P2P 组单独建一个网络接口。很多驱动
//! （比如 iwlwifi）不支持在那个接口上切到 P2P 模式，会报
//! `Failed to set P2P Mode: ret=-95`。把 `NoGroupIface` 设成 true，组就
//! 直接跑在主接口上，这样能避开驱动的这个限制。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::Mutex;
use zbus::zvariant::{OwnedValue, Value};

/// wpa_supplicant 的 D-Bus 服务名。
const WPA_SERVICE: &str = "fi.w1.wpa_supplicant1";

/// P2P 组所有者的 IP：互传联盟固定用 192.168.49.0/24，网关是 `.1`。
pub const GO_ADDRESS: &str = "192.168.49.1";

/// P2P 组的网段长度。
pub const GO_PREFIX_LENGTH: u8 = 24;

/// 需要提权的命令最多等这么久（`pkexec` 可能弹认证框）。
const PRIVILEGED_TIMEOUT: Duration = Duration::from_secs(120);

/// `fi.w1.wpa_supplicant1.Interface.P2PDevice` 接口。
const P2P_DEVICE_INTERFACE: &str = "fi.w1.wpa_supplicant1.Interface.P2PDevice";

/// `fi.w1.wpa_supplicant1.Group` 接口。
const GROUP_INTERFACE: &str = "fi.w1.wpa_supplicant1.Group";

/// `fi.w1.wpa_supplicant1.Interface` 接口。
const INTERFACE_INTERFACE: &str = "fi.w1.wpa_supplicant1.Interface";

/// 一个 P2P 组的信息。
#[derive(Debug, Clone)]
pub struct P2pGroup {
    /// 组名，格式是 `DIRECT-xx`。
    pub ssid: String,
    /// 组密码。
    pub psk: String,
    /// 组所有者接口的名字。
    pub interface: String,
    /// 组的频率（MHz）。
    pub frequency: u32,
}

/// `satella-mta-helper create` 的 JSON 输出。
///
/// 父进程和 helper 共用这个定义，避免两边字段名漂移。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelperGroupInfo {
    pub ssid: String,
    pub psk: String,
    pub interface: String,
    pub frequency: u32,
    pub dhcp_pid: u32,
}

/// 用 `wpa_supplicant` 的 D-Bus 接口创建一个 P2P 组。
pub struct WpaP2pGroupOwner {
    connection: zbus::Connection,
    interface_path: String,
}

impl WpaP2pGroupOwner {
    /// 连接到 `wpa_supplicant` 并找到主接口。
    pub async fn new(interface: &str) -> anyhow::Result<Self> {
        let connection = zbus::Connection::system().await?;

        // 主接口（wlan0）的路径。
        let proxy = zbus::Proxy::new(
            &connection,
            WPA_SERVICE,
            "/fi/w1/wpa_supplicant1",
            WPA_SERVICE,
        )
        .await?;

        let interfaces: OwnedValue = proxy.get_property("Interfaces").await?;
        let interface_path = find_interface(&connection, &interfaces, interface)
            .await?
            .ok_or_else(|| anyhow::anyhow!("wpa_supplicant does not manage {interface}"))?;

        let owner = Self {
            connection,
            interface_path,
        };

        owner.set_no_group_iface().await?;

        Ok(owner)
    }

    /// 让 P2P 组直接跑在主接口上。
    ///
    /// 单独的组接口在很多驱动上切不到 P2P 模式（`ret=-95`）。
    async fn set_no_group_iface(&self) -> anyhow::Result<()> {
        let proxy = self.p2p_proxy().await?;

        // 写属性时只提交要改的键，wpa_supplicant 会保留其余配置。
        let config: HashMap<String, Value<'static>> =
            HashMap::from([("NoGroupIface".to_owned(), Value::Bool(true))]);

        proxy
            .set_property("P2PDeviceConfig", config)
            .await
            .map_err(|err| anyhow::anyhow!("could not set NoGroupIface: {err}"))?;

        debug!("Set NoGroupIface, so the P2P group runs on the main interface");
        Ok(())
    }

    async fn p2p_proxy(&self) -> anyhow::Result<zbus::Proxy<'_>> {
        Ok(zbus::Proxy::new(
            &self.connection,
            WPA_SERVICE,
            self.interface_path.as_str(),
            P2P_DEVICE_INTERFACE,
        )
        .await?)
    }

    async fn interface_proxy(&self) -> anyhow::Result<zbus::Proxy<'_>> {
        Ok(zbus::Proxy::new(
            &self.connection,
            WPA_SERVICE,
            self.interface_path.as_str(),
            INTERFACE_INTERFACE,
        )
        .await?)
    }

    /// 创建一个 P2P 组，等待它真的建立起来。
    pub async fn create_group(
        &self,
        frequency: u32,
        timeout: Duration,
    ) -> anyhow::Result<P2pGroup> {
        let proxy = self.p2p_proxy().await?;

        // 之前的组可能还在，先拆掉。
        let _ = proxy.call_method("Disconnect", &()).await;

        // D-Bus 的 `GroupAdd` 把 frequency 定义成有符号 32 位整数，
        // 这与 `Frequency` 属性的 `q` 不同。
        let frequency = i32::try_from(frequency)
            .map_err(|_| anyhow::anyhow!("frequency {frequency} does not fit in an i32"))?;
        let args: HashMap<String, Value<'static>> =
            HashMap::from([("frequency".to_owned(), Value::I32(frequency))]);

        proxy
            .call_method("GroupAdd", &args)
            .await
            .map_err(|err| anyhow::anyhow!("could not create the P2P group: {err}"))?;

        let group = self.wait_for_group(timeout).await?;

        info!(
            "Created the P2P group {} on channel {}",
            group.ssid, group.frequency
        );

        Ok(group)
    }

    /// 等组建立起来，然后读出它的 SSID 和密码。
    async fn wait_for_group(&self, timeout: Duration) -> anyhow::Result<P2pGroup> {
        let deadline = Instant::now() + timeout;

        loop {
            if let Some(group) = self.current_group().await? {
                return Ok(group);
            }

            if Instant::now() >= deadline {
                return Err(anyhow::anyhow!(
                    "the P2P group did not start within {timeout:?}"
                ));
            }

            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// 读当前这个组的信息。
    async fn current_group(&self) -> anyhow::Result<Option<P2pGroup>> {
        let proxy = self.p2p_proxy().await?;

        let role: OwnedValue = proxy.get_property("Role").await?;
        if string_from_value(&role).as_deref() != Some("GO") {
            return Ok(None);
        }

        let group_path: OwnedValue = proxy.get_property("Group").await?;
        let Some(group_path) = object_path_from_value(&group_path) else {
            return Ok(None);
        };
        if group_path == "/" {
            return Ok(None);
        }

        // 组对象在 `fi.w1.wpa_supplicant1` 上。
        let group_proxy = zbus::Proxy::new(
            &self.connection,
            WPA_SERVICE,
            group_path.as_str(),
            GROUP_INTERFACE,
        )
        .await?;

        let group_role: OwnedValue = group_proxy.get_property("Role").await?;
        if string_from_value(&group_role).as_deref() != Some("GO") {
            return Ok(None);
        }

        let ssid: OwnedValue = group_proxy.get_property("SSID").await?;
        let psk: OwnedValue = group_proxy.get_property("Passphrase").await?;
        let frequency: OwnedValue = group_proxy.get_property("Frequency").await?;

        let ifname: OwnedValue = self.interface_proxy().await?.get_property("Ifname").await?;

        Ok(Some(P2pGroup {
            ssid: bytes_to_string(&ssid).unwrap_or_default(),
            psk: string_from_value(&psk).unwrap_or_default(),
            interface: string_from_value(&ifname).unwrap_or_default(),
            frequency: u16_from_value(&frequency).map(u32::from).unwrap_or(0),
        }))
    }

    /// 拆掉这个组。
    pub async fn remove_group(&self) -> anyhow::Result<()> {
        let proxy = self.p2p_proxy().await?;
        if let Err(err) = proxy.call_method("Disconnect", &()).await {
            warn!("could not remove the P2P group: {err}");
        }
        Ok(())
    }
}

/// 创建/销毁 P2P 组所需要的那一小部分特权操作。
///
/// Linux 上有两个实现：已经以 root 运行时直接在进程内和 `wpa_supplicant`
/// 说话（[`InProcessGroupOwner`]）；普通用户运行时调用一次性的
/// [`HelperGroupOwner`]。将来的 Windows/macOS 后端只要实现这个 trait，
/// 发送流程就不用跟着改。
#[async_trait]
pub trait GroupOwnerBackend: Send + Sync {
    /// 在指定频率上创建组，并配好地址和 DHCP。
    async fn create_group(&self, frequency: u32) -> anyhow::Result<P2pGroup>;

    /// 拆掉组、停掉 DHCP，并去掉地址。
    async fn destroy_group(&self) -> anyhow::Result<()>;
}

/// 已经以 root 运行时使用的后端。
pub struct InProcessGroupOwner {
    owner: WpaP2pGroupOwner,
    state: Mutex<Option<InProcessGroupState>>,
}

struct InProcessGroupState {
    group: P2pGroup,
    dhcp: DhcpServer,
}

impl InProcessGroupOwner {
    /// 连接到 `wpa_supplicant`，准备在 `interface` 上建组。
    pub async fn new(interface: &str) -> anyhow::Result<Self> {
        Ok(Self {
            owner: WpaP2pGroupOwner::new(interface).await?,
            state: Mutex::new(None),
        })
    }
}

#[async_trait]
impl GroupOwnerBackend for InProcessGroupOwner {
    async fn create_group(&self, frequency: u32) -> anyhow::Result<P2pGroup> {
        let has_state = { self.state.lock().await.is_some() };
        if has_state {
            self.destroy_group().await?;
        }

        let group = self
            .owner
            .create_group(frequency, Duration::from_secs(20))
            .await?;

        if group.ssid.is_empty() || group.psk.is_empty() {
            let _ = self.owner.remove_group().await;
            return Err(anyhow::anyhow!(
                "the P2P group has an empty SSID or passphrase"
            ));
        }

        if let Err(err) = configure_go_interface(&group.interface).await {
            let _ = self.owner.remove_group().await;
            return Err(err);
        }

        let dhcp = match DhcpServer::start(&group.interface).await {
            Ok(dhcp) => dhcp,
            Err(err) => {
                let _ = remove_go_interface_address(&group.interface).await;
                let _ = self.owner.remove_group().await;
                return Err(err);
            }
        };

        *self.state.lock().await = Some(InProcessGroupState {
            group: group.clone(),
            dhcp,
        });

        Ok(group)
    }

    async fn destroy_group(&self) -> anyhow::Result<()> {
        let Some(state) = self.state.lock().await.take() else {
            return Ok(());
        };

        let mut first_error = None;
        if let Err(err) = state.dhcp.stop().await {
            first_error = Some(err);
        }
        if let Err(err) = self.owner.remove_group().await
            && first_error.is_none()
        {
            first_error = Some(err);
        }
        if let Err(err) = remove_go_interface_address(&state.group.interface).await
            && first_error.is_none()
        {
            first_error = Some(err);
        }

        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

/// 普通用户运行时使用的后端：每次操作调用一次 `satella-mta-helper`。
///
/// helper 跑完就退出；P2P 组和 `dnsmasq` 留在系统里，所以父进程不能在
/// helper 退出后就认为组没了。
pub struct HelperGroupOwner {
    interface: String,
    helper: PathBuf,
    state: Mutex<Option<HelperGroupInfo>>,
}

impl HelperGroupOwner {
    /// 找到 helper，准备用它管理 `interface` 上的组。
    pub fn new(interface: &str) -> anyhow::Result<Self> {
        let helper = resolve_helper_path().ok_or_else(|| {
            anyhow::anyhow!(
                "could not find satella-mta-helper (set SATELLA_MTA_HELPER, build it next to \
                 satella-cli, or install it in /usr/libexec)"
            )
        })?;

        Ok(Self {
            interface: interface.to_owned(),
            helper,
            state: Mutex::new(None),
        })
    }

    async fn destroy_info(&self, info: &HelperGroupInfo) -> anyhow::Result<()> {
        let args = vec![
            "destroy".to_owned(),
            "--iface".to_owned(),
            info.interface.clone(),
            "--dhcp-pid".to_owned(),
            info.dhcp_pid.to_string(),
        ];

        run_privileged(&self.helper.display().to_string(), &args).await?;
        Ok(())
    }
}

#[async_trait]
impl GroupOwnerBackend for HelperGroupOwner {
    async fn create_group(&self, frequency: u32) -> anyhow::Result<P2pGroup> {
        let has_state = { self.state.lock().await.is_some() };
        if has_state {
            self.destroy_group().await?;
        }

        let args = vec![
            "create".to_owned(),
            "--iface".to_owned(),
            self.interface.clone(),
            "--freq".to_owned(),
            frequency.to_string(),
        ];

        let stdout = run_privileged(&self.helper.display().to_string(), &args).await?;
        let info: HelperGroupInfo = serde_json::from_str(stdout.trim()).map_err(|err| {
            anyhow::anyhow!(
                "satella-mta-helper returned invalid JSON ({err}): {}",
                stdout.trim()
            )
        })?;

        validate_interface_name(&info.interface)?;

        if info.ssid.is_empty() || info.psk.is_empty() {
            let _ = self.destroy_info(&info).await;
            return Err(anyhow::anyhow!(
                "satella-mta-helper returned an empty SSID or passphrase"
            ));
        }
        validate_frequency(info.frequency)?;

        let group = P2pGroup {
            ssid: info.ssid.clone(),
            psk: info.psk.clone(),
            interface: info.interface.clone(),
            frequency: info.frequency,
        };
        *self.state.lock().await = Some(info);

        Ok(group)
    }

    async fn destroy_group(&self) -> anyhow::Result<()> {
        let Some(info) = self.state.lock().await.take() else {
            return Ok(());
        };

        self.destroy_info(&info).await
    }
}

/// helper 的文件名。
const HELPER_NAME: &str = "satella-mta-helper";

/// 找到 `satella-mta-helper`。
///
/// 顺序：`SATELLA_MTA_HELPER`、当前可执行文件旁边（`cargo test` 时再上一层
/// 的 `deps`）、最后是打包安装的 `/usr/libexec`。
fn resolve_helper_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SATELLA_MTA_HELPER") {
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            match std::env::current_dir() {
                Ok(directory) => directory.join(path),
                Err(err) => {
                    warn!(
                        "SATELLA_MTA_HELPER is relative and the current directory is unknown: {err}"
                    );
                    return None;
                }
            }
        };

        if is_executable(&path) {
            return Some(path);
        }
        warn!(
            "SATELLA_MTA_HELPER={} is not an executable file",
            path.display()
        );
    }

    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        let candidate = parent.join(HELPER_NAME);
        if is_executable(&candidate) {
            return Some(candidate);
        }

        // `cargo test` 从 `target/debug/deps` 跑，helper 在上一层。
        if parent.file_name().is_some_and(|name| name == "deps")
            && let Some(parent) = parent.parent()
        {
            let candidate = parent.join(HELPER_NAME);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }

    let installed = PathBuf::from("/usr/libexec").join(HELPER_NAME);
    if is_executable(&installed) {
        return Some(installed);
    }

    None
}

/// 文件存在且带可执行位。
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// helper 只接受这样的接口名：`wlan*` 或 `p2p-*`，最长 15 字节。
pub fn validate_interface_name(interface: &str) -> anyhow::Result<()> {
    if interface.is_empty() || interface.len() > 15 {
        return Err(anyhow::anyhow!(
            "interface name must be 1-15 bytes, got {interface:?}"
        ));
    }

    if !(interface.starts_with("wlan") || interface.starts_with("p2p-")) {
        return Err(anyhow::anyhow!(
            "interface {interface:?} must start with wlan or p2p-"
        ));
    }

    let safe_characters = interface
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));

    if !safe_characters || interface.contains("..") || interface.starts_with('.') {
        return Err(anyhow::anyhow!(
            "interface {interface:?} contains unsafe characters"
        ));
    }

    Ok(())
}

/// helper 只接受 2.4 GHz 和 5 GHz 的信道。
pub fn validate_frequency(frequency: u32) -> anyhow::Result<()> {
    if (2400..=2500).contains(&frequency) || (4900..=5900).contains(&frequency) {
        return Ok(());
    }

    Err(anyhow::anyhow!(
        "frequency {frequency} MHz is outside the 2.4 GHz and 5 GHz Wi-Fi bands"
    ))
}

/// 给组所有者的接口配上互传联盟的地址。
///
/// P2P 组建立以后 `wpa_supplicant` 可能重置接口，所以地址要在组起来之后再配。
pub async fn configure_go_interface(interface: &str) -> anyhow::Result<()> {
    let address = format!("{GO_ADDRESS}/{GO_PREFIX_LENGTH}");

    run_privileged(
        "ip",
        &[
            "addr".to_owned(),
            "replace".to_owned(),
            address,
            "dev".to_owned(),
            interface.to_owned(),
        ],
    )
    .await?;

    run_privileged(
        "ip",
        &[
            "link".to_owned(),
            "set".to_owned(),
            interface.to_owned(),
            "up".to_owned(),
        ],
    )
    .await?;

    info!("Configured {interface} as {GO_ADDRESS}/{GO_PREFIX_LENGTH}");
    Ok(())
}

/// 去掉组所有者接口上的地址。
///
/// 地址已经不在时也算成功：清理代码会在失败路径上重复调用它。
pub async fn remove_go_interface_address(interface: &str) -> anyhow::Result<()> {
    let address = format!("{GO_ADDRESS}/{GO_PREFIX_LENGTH}");

    match run_privileged(
        "ip",
        &[
            "addr".to_owned(),
            "del".to_owned(),
            address,
            "dev".to_owned(),
            interface.to_owned(),
        ],
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(err) if is_absent_link_error(&err) => {
            debug!("{GO_ADDRESS}/{GO_PREFIX_LENGTH} is already absent from {interface}");
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// `ip` 是不是在说“这个地址/接口本来就没了”？
fn is_absent_link_error(err: &anyhow::Error) -> bool {
    let message = err.to_string();
    message.contains("Cannot assign requested address") || message.contains("Cannot find device")
}

/// 组所有者上的 DHCP 服务器。
///
/// 接收端加入组以后会先发 DHCP 请求，拿到 `192.168.49.2` 之类的地址，
/// 再用网关地址 `192.168.49.1` 连我们的 HTTPS 服务器。没有 DHCP，接收端
/// 会一直停在“正在连接”。
pub struct DhcpServer {
    interface: String,
    pid: u32,
}

impl DhcpServer {
    /// 在接口上启动 `dnsmasq`，只做 DHCP，不做 DNS。
    pub async fn start(interface: &str) -> anyhow::Result<Self> {
        let directory = runtime_directory()?;

        let pid_file = directory.join(format!("dnsmasq-{interface}.pid"));
        let _ = std::fs::remove_file(&pid_file);

        let args = vec![
            // 不读系统配置：只跑我们这一个 DHCP 服务器。
            "--conf-file=/dev/null".to_owned(),
            // 不要 DNS，端口 53 留给别人。
            "--port=0".to_owned(),
            // 组是临时的，租约不用落盘；也避免 dnsmasq 降权后写不了文件。
            "--dhcp-leasefile=/dev/null".to_owned(),
            // 不降权，保证 pid 文件和 DHCP 套接字都能正常用。
            "--user=root".to_owned(),
            format!("--interface={interface}"),
            "--bind-interfaces".to_owned(),
            "--dhcp-range=192.168.49.2,192.168.49.254,255.255.255.0,12h".to_owned(),
            format!("--dhcp-option=3,{GO_ADDRESS}"),
            format!("--dhcp-option=6,{GO_ADDRESS}"),
            format!("--pid-file={}", pid_file.display()),
        ];

        run_privileged("dnsmasq", &args).await?;

        // dnsmasq 会先 fork 再写 pid 文件，等它真的起来。
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(contents) = std::fs::read_to_string(&pid_file) {
                let contents = contents.trim();
                if !contents.is_empty() {
                    let pid: u32 = contents.parse().map_err(|err| {
                        anyhow::anyhow!(
                            "dnsmasq wrote an invalid pid {contents:?} to {}: {err}",
                            pid_file.display()
                        )
                    })?;

                    info!("Started DHCP on {interface} (dnsmasq pid {pid})");
                    return Ok(Self {
                        interface: interface.to_owned(),
                        pid,
                    });
                }
            }

            if Instant::now() >= deadline {
                return Err(anyhow::anyhow!(
                    "dnsmasq did not write its pid file at {}",
                    pid_file.display()
                ));
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// `dnsmasq` 的进程号。
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// 停掉 DHCP 服务器。
    pub async fn stop(&self) -> anyhow::Result<()> {
        Self::stop_pid(&self.interface, Some(self.pid)).await
    }

    /// 按 root 拥有的 pid 文件停掉接口上的 `dnsmasq`。
    ///
    /// `expected_pid` 只用来发现调用方拿错了数字：真正被杀的 PID 永远来自
    /// pid 文件，所以调用方没法借这个接口去杀别的进程。文件不存在、内容
    /// 坏掉、或者 PID 已经不是 `dnsmasq` 时都直接跳过，这样清理是幂等的。
    pub async fn stop_pid(interface: &str, expected_pid: Option<u32>) -> anyhow::Result<()> {
        let directory = runtime_directory()?;
        let pid_file = directory.join(format!("dnsmasq-{interface}.pid"));

        let contents = match std::fs::read_to_string(&pid_file) {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                debug!(
                    "No DHCP pid file at {}; nothing to stop",
                    pid_file.display()
                );
                return Ok(());
            }
            Err(err) => {
                return Err(anyhow::anyhow!(
                    "could not read the DHCP pid file {}: {err}",
                    pid_file.display()
                ));
            }
        };

        let pid: u32 = match contents.trim().parse() {
            Ok(pid) => pid,
            Err(err) => {
                warn!(
                    "Ignoring the malformed DHCP pid file {}: {err}",
                    pid_file.display()
                );
                let _ = std::fs::remove_file(&pid_file);
                return Ok(());
            }
        };

        if let Some(expected) = expected_pid
            && expected != pid
        {
            warn!("DHCP pid file says {pid}, caller expected {expected}; using the pid file");
        }

        if !is_dnsmasq(pid) {
            warn!(
                "PID {pid} from {} is not dnsmasq; removing the stale pid file",
                pid_file.display()
            );
            let _ = std::fs::remove_file(&pid_file);
            return Ok(());
        }

        if let Err(err) = run_privileged("kill", &[pid.to_string()]).await {
            warn!("could not stop dnsmasq (pid {pid}) on {interface}: {err}");
        }

        let _ = std::fs::remove_file(&pid_file);
        Ok(())
    }
}

/// 这个 PID 现在还是不是 `dnsmasq`？
///
/// `/proc` 读不到时按“不是”处理：宁可留下一个进程，也不误杀别的程序。
fn is_dnsmasq(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|comm| comm.trim() == "dnsmasq")
        .unwrap_or(false)
}

/// 运行一个需要 root 的命令。
///
/// 顺序是：本来就是 root 就直接跑；否则先试免密的 `sudo -n`；再不行就让
/// `pkexec` 弹认证框。这样 `satella-cli` 既能在 root 下无感运行，也能在
/// 普通用户下完成一次提权。
pub(crate) async fn run_privileged(program: &str, args: &[String]) -> anyhow::Result<String> {
    let path = resolve_program(program);

    let mut command = if is_root() {
        Command::new(&path)
    } else if sudo_available().await {
        let mut command = Command::new(resolve_program("sudo"));
        command.args(["-n", &path]);
        command
    } else {
        let mut command = Command::new(resolve_program("pkexec"));
        command.arg(&path);
        command
    };

    command.args(args).stdin(Stdio::null());

    let output = output_with_timeout(command, program, PRIVILEGED_TIMEOUT).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };

        return Err(anyhow::anyhow!(
            "{program} exited with {}: {detail}",
            output.status
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 当前进程是不是 root。
pub fn is_root() -> bool {
    // SAFETY: geteuid 没有前置条件，也不会失败。
    unsafe { libc::geteuid() == 0 }
}

/// `sudo -n true` 能不能直接过（即不需要密码）。
async fn sudo_available() -> bool {
    let mut command = Command::new(resolve_program("sudo"));
    command.args(["-n", "true"]).stdin(Stdio::null());

    matches!(
        output_with_timeout(command, "sudo", Duration::from_secs(5)).await,
        Ok(output) if output.status.success()
    )
}

/// 跑一个命令，超时就杀掉。
async fn output_with_timeout(
    mut command: Command,
    program: &str,
    timeout: Duration,
) -> anyhow::Result<std::process::Output> {
    command.kill_on_drop(true);

    match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(err)) => Err(anyhow::anyhow!("could not run {program}: {err}")),
        Err(_) => Err(anyhow::anyhow!(
            "{program} did not finish within {timeout:?} (is an authorization dialog waiting?)"
        )),
    }
}

/// 找到命令的绝对路径。
///
/// `pkexec` 和 `sudo` 在清理过的环境里跑，用绝对路径最稳。
fn resolve_program(program: &str) -> String {
    if program.contains('/') {
        return program.to_owned();
    }

    for directory in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        let candidate = Path::new(directory).join(program);
        if candidate.exists() {
            return candidate.display().to_string();
        }
    }

    program.to_owned()
}

/// 放 pid 文件的目录。
///
/// root 下必须落在 `/run`：那个目录 root 拥有、`0700`，pid 文件才可信。
/// 普通用户创建组时不会走到 DHCP（组所有者本来就要求 root），但如果真
/// 走到这里，仍然允许退回临时目录。
fn runtime_directory() -> anyhow::Result<PathBuf> {
    let preferred = PathBuf::from("/run/satella-mta");
    match std::fs::create_dir_all(&preferred) {
        Ok(()) => {
            if is_root() {
                harden_runtime_directory(&preferred)?;
            }
            return Ok(preferred);
        }
        Err(err) if is_root() => {
            return Err(anyhow::anyhow!(
                "could not create the runtime directory {}: {err}",
                preferred.display()
            ));
        }
        Err(_) => {}
    }

    let fallback = std::env::temp_dir().join("satella-mta");
    std::fs::create_dir_all(&fallback).map_err(|err| {
        anyhow::anyhow!(
            "could not create the runtime directory {}: {err}",
            fallback.display()
        )
    })?;
    Ok(fallback)
}

/// 确保 root 用的运行目录只属于 root。
fn harden_runtime_directory(directory: &Path) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() {
        return Err(anyhow::anyhow!(
            "{} is not a directory",
            directory.display()
        ));
    }

    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;

    let path = std::ffi::CString::new(directory.as_os_str().as_bytes())
        .map_err(|_| anyhow::anyhow!("{} contains a NUL byte", directory.display()))?;
    // SAFETY: `path` is a valid NUL-terminated string and `chown` does not keep
    // the pointer after it returns.
    if unsafe { libc::chown(path.as_ptr(), 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }

    Ok(())
}

/// 在 `Interfaces` 属性里找到名为 `interface` 的那个对象路径。
async fn find_interface(
    connection: &zbus::Connection,
    interfaces: &OwnedValue,
    interface: &str,
) -> anyhow::Result<Option<String>> {
    for path in object_paths_from_value(interfaces) {
        let proxy =
            zbus::Proxy::new(connection, WPA_SERVICE, path.as_str(), INTERFACE_INTERFACE).await?;

        let ifname: OwnedValue = proxy.get_property("Ifname").await?;
        if string_from_value(&ifname).as_deref() == Some(interface) {
            return Ok(Some(path));
        }
    }

    Ok(None)
}

/// 从 `ao` 类型（`OwnedValue` 里的数组）取出对象路径。
fn object_paths_from_value(value: &OwnedValue) -> Vec<String> {
    let Ok(array) = value.downcast_ref::<zbus::zvariant::Array<'_>>() else {
        return Vec::new();
    };

    array
        .iter()
        .filter_map(|item| item.downcast_ref::<zbus::zvariant::ObjectPath<'_>>().ok())
        .map(|path| path.as_str().to_owned())
        .collect()
}

/// 从值里取出对象路径。
fn object_path_from_value(value: &OwnedValue) -> Option<String> {
    value
        .downcast_ref::<zbus::zvariant::ObjectPath<'_>>()
        .ok()
        .map(|path| path.as_str().to_owned())
}

/// 从值里取出字符串。
fn string_from_value(value: &OwnedValue) -> Option<String> {
    value
        .downcast_ref::<zbus::zvariant::Str<'_>>()
        .ok()
        .map(|text| text.as_str().to_owned())
}

/// 从值里取出字节数组并当成文本。
fn bytes_to_string(value: &OwnedValue) -> Option<String> {
    let array = value.downcast_ref::<zbus::zvariant::Array<'_>>().ok()?;
    let bytes = array
        .iter()
        .map(|item| item.downcast_ref::<u8>())
        .collect::<Result<Vec<_>, _>>()
        .ok()?;

    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// 从值里取出 u16。
fn u16_from_value(value: &OwnedValue) -> Option<u16> {
    value.downcast_ref::<u16>().ok()
}
