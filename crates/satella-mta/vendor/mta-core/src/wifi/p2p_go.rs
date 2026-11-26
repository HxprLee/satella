//! WiFi P2P group owner
//!
//! An MTA sender must be a real WiFi P2P group owner: the peer does not join
//! an ordinary access point, it only runs `p2p_connect` to join a P2P group
//! named `DIRECT-xx`.
//!
//! # Why not NetworkManager
//!
//! NetworkManager can only create ordinary access points (APs); it has no
//! interface for creating a P2P group owner. This module therefore talks to
//! `wpa_supplicant` directly.
//!
//! # Why D-Bus instead of wpa_cli
//!
//! Only root can access `wpa_supplicant`'s control socket `/run/wpa_supplicant`,
//! and its D-Bus interface `fi.w1.wpa_supplicant1` has the same restriction.
//! Both paths therefore need privilege; D-Bus is better because it returns the
//! group information (SSID and passphrase) directly instead of making us guess.
//!
//! # NoGroupIface
//!
//! By default `wpa_supplicant` creates a separate network interface for the P2P
//! group. Many drivers (iwlwifi, for example) cannot switch that interface to
//! P2P mode and report `Failed to set P2P Mode: ret=-95`. Setting
//! `NoGroupIface` to true makes the group run on the main interface instead,
//! which avoids this driver limitation.

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

/// The D-Bus service name of `wpa_supplicant`.
const WPA_SERVICE: &str = "fi.w1.wpa_supplicant1";

/// IP address of the P2P group owner: MTA always uses 192.168.49.0/24, with
/// `.1` as the gateway.
pub const GO_ADDRESS: &str = "192.168.49.1";

/// Prefix length of the P2P group subnet.
pub const GO_PREFIX_LENGTH: u8 = 24;

/// Maximum time to wait for a privileged command (`pkexec` may show an
/// authentication dialog).
const PRIVILEGED_TIMEOUT: Duration = Duration::from_secs(120);

/// The `fi.w1.wpa_supplicant1.Interface.P2PDevice` interface.
const P2P_DEVICE_INTERFACE: &str = "fi.w1.wpa_supplicant1.Interface.P2PDevice";

/// The `fi.w1.wpa_supplicant1.Group` interface.
const GROUP_INTERFACE: &str = "fi.w1.wpa_supplicant1.Group";

/// The `fi.w1.wpa_supplicant1.Interface` interface.
const INTERFACE_INTERFACE: &str = "fi.w1.wpa_supplicant1.Interface";

/// Information about a P2P group.
#[derive(Debug, Clone)]
pub struct P2pGroup {
    /// Group name, in the form `DIRECT-xx`.
    pub ssid: String,
    /// Group passphrase.
    pub psk: String,
    /// Name of the group owner interface.
    pub interface: String,
    /// Group frequency in MHz.
    pub frequency: u32,
}

/// JSON output of `satella-mta-helper create`.
///
/// The parent process and the helper share this definition so their field
/// names cannot drift apart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelperGroupInfo {
    pub ssid: String,
    pub psk: String,
    pub interface: String,
    pub frequency: u32,
    pub dhcp_pid: u32,
}

/// Creates a P2P group through `wpa_supplicant`'s D-Bus interface.
pub struct WpaP2pGroupOwner {
    connection: zbus::Connection,
    interface_path: String,
}

impl WpaP2pGroupOwner {
    /// Connect to `wpa_supplicant` and find the main interface.
    pub async fn new(interface: &str) -> anyhow::Result<Self> {
        let connection = zbus::Connection::system().await?;

        // Path of the main interface (wlan0).
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

    /// Make the P2P group run directly on the main interface.
    ///
    /// A separate group interface cannot switch to P2P mode on many drivers
    /// (`ret=-95`).
    async fn set_no_group_iface(&self) -> anyhow::Result<()> {
        let proxy = self.p2p_proxy().await?;

        // Only submit the key being changed; wpa_supplicant keeps the rest of
        // the configuration.
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

    /// Create a P2P group and wait until it is actually established.
    pub async fn create_group(
        &self,
        frequency: u32,
        timeout: Duration,
    ) -> anyhow::Result<P2pGroup> {
        let proxy = self.p2p_proxy().await?;

        // A previous group may still exist; tear it down first.
        let _ = proxy.call_method("Disconnect", &()).await;

        // D-Bus `GroupAdd` defines frequency as a signed 32-bit integer,
        // unlike the `q` used by the `Frequency` property.
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

    /// Wait for the group to be established, then read its SSID and passphrase.
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

    /// Read the information of the current group.
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

        // The group object lives under `fi.w1.wpa_supplicant1`.
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

    /// Tear down this group.
    pub async fn remove_group(&self) -> anyhow::Result<()> {
        let proxy = self.p2p_proxy().await?;
        if let Err(err) = proxy.call_method("Disconnect", &()).await {
            warn!("could not remove the P2P group: {err}");
        }
        Ok(())
    }
}

/// The small set of privileged operations needed to create and destroy a P2P
/// group.
///
/// Linux has two implementations: when already running as root, talk to
/// `wpa_supplicant` in-process ([`InProcessGroupOwner`]); when running as an
/// ordinary user, call the one-shot [`HelperGroupOwner`]. A future
/// Windows/macOS backend only needs to implement this trait, and the send
/// workflow does not have to change.
#[async_trait]
pub trait GroupOwnerBackend: Send + Sync {
    /// Create a group on the given frequency, configure its address, and start
    /// DHCP.
    async fn create_group(&self, frequency: u32) -> anyhow::Result<P2pGroup>;

    /// Tear down the group, stop DHCP, and remove the address.
    async fn destroy_group(&self) -> anyhow::Result<()>;
}

/// Backend used when already running as root.
pub struct InProcessGroupOwner {
    owner: WpaP2pGroupOwner,
    state: Mutex<Option<InProcessGroupState>>,
}

struct InProcessGroupState {
    group: P2pGroup,
    dhcp: DhcpServer,
}

impl InProcessGroupOwner {
    /// Connect to `wpa_supplicant`, ready to create a group on `interface`.
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

/// Backend used when running as an ordinary user: calls `satella-mta-helper`
/// once per operation.
///
/// The helper exits after each run; the P2P group and `dnsmasq` stay in the
/// system, so the parent process must not assume the group is gone just because
/// the helper exited.
pub struct HelperGroupOwner {
    interface: String,
    helper: PathBuf,
    state: Mutex<Option<HelperGroupInfo>>,
}

impl HelperGroupOwner {
    /// Find the helper, ready to use it to manage the group on `interface`.
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

/// File name of the helper.
const HELPER_NAME: &str = "satella-mta-helper";

/// Find `satella-mta-helper`.
///
/// Search order: `SATELLA_MTA_HELPER`, next to the current executable (and one
/// level up from `deps` when running `cargo test`), then the packaged install
/// location `/usr/libexec`.
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

        // `cargo test` runs from `target/debug/deps`, so the helper is one
        // level up.
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

/// Whether the file exists and has an executable bit set.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// The helper only accepts interface names of the form `wlan*` or `p2p-*`,
/// at most 15 bytes long.
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

/// The helper only accepts channels in the 2.4 GHz and 5 GHz bands.
pub fn validate_frequency(frequency: u32) -> anyhow::Result<()> {
    if (2400..=2500).contains(&frequency) || (4900..=5900).contains(&frequency) {
        return Ok(());
    }

    Err(anyhow::anyhow!(
        "frequency {frequency} MHz is outside the 2.4 GHz and 5 GHz Wi-Fi bands"
    ))
}

/// Configure the group owner interface with the MTA address.
///
/// `wpa_supplicant` may reset the interface after the P2P group is created, so
/// the address has to be configured after the group is up.
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

/// Remove the address from the group owner interface.
///
/// This counts as success when the address is already gone: cleanup code calls
/// it repeatedly on failure paths.
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

/// Is `ip` saying that this address or interface was already absent?
fn is_absent_link_error(err: &anyhow::Error) -> bool {
    let message = err.to_string();
    message.contains("Cannot assign requested address") || message.contains("Cannot find device")
}

/// DHCP server on the group owner.
///
/// After joining the group, the receiver first sends a DHCP request and gets
/// an address such as `192.168.49.2`, then uses the gateway address
/// `192.168.49.1` to connect to our HTTPS server. Without DHCP the receiver
/// stays stuck at "connecting".
pub struct DhcpServer {
    interface: String,
    pid: u32,
}

impl DhcpServer {
    /// Start `dnsmasq` on the interface, serving DHCP only, not DNS.
    pub async fn start(interface: &str) -> anyhow::Result<Self> {
        let directory = runtime_directory()?;

        let pid_file = directory.join(format!("dnsmasq-{interface}.pid"));
        let _ = std::fs::remove_file(&pid_file);

        let args = vec![
            // Do not read the system configuration: run only this DHCP server.
            "--conf-file=/dev/null".to_owned(),
            // No DNS; leave port 53 to everyone else.
            "--port=0".to_owned(),
            // The group is temporary, so leases do not need to be persisted;
            // this also avoids write failures after dnsmasq drops privileges.
            "--dhcp-leasefile=/dev/null".to_owned(),
            // Do not drop privileges, so the pid file and the DHCP socket work.
            "--user=root".to_owned(),
            format!("--interface={interface}"),
            "--bind-interfaces".to_owned(),
            "--dhcp-range=192.168.49.2,192.168.49.254,255.255.255.0,12h".to_owned(),
            format!("--dhcp-option=3,{GO_ADDRESS}"),
            format!("--dhcp-option=6,{GO_ADDRESS}"),
            format!("--pid-file={}", pid_file.display()),
        ];

        run_privileged("dnsmasq", &args).await?;

        // dnsmasq forks before writing its pid file; wait until it is really up.
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

    /// Process ID of `dnsmasq`.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Stop the DHCP server.
    pub async fn stop(&self) -> anyhow::Result<()> {
        Self::stop_pid(&self.interface, Some(self.pid)).await
    }

    /// Stop `dnsmasq` on the interface using the root-owned pid file.
    ///
    /// `expected_pid` is only used to detect that the caller passed the wrong
    /// number: the PID that is actually killed always comes from the pid file,
    /// so callers cannot use this interface to kill another process. When the
    /// file is missing, its contents are malformed, or the PID no longer
    /// belongs to `dnsmasq`, this is skipped, which makes cleanup idempotent.
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

/// Is this PID still `dnsmasq`?
///
/// When `/proc` cannot be read, treat it as "no": better to leave a process
/// behind than to kill the wrong program.
fn is_dnsmasq(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|comm| comm.trim() == "dnsmasq")
        .unwrap_or(false)
}

/// Run a command that requires root.
///
/// Order: if already root, run it directly; otherwise try passwordless
/// `sudo -n`; if that fails, let `pkexec` show an authentication dialog. This
/// lets `satella-cli` run transparently as root and still elevate once as an
/// ordinary user.
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

/// Whether the current process is root.
pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// Whether `sudo -n true` succeeds directly (that is, without a password).
async fn sudo_available() -> bool {
    let mut command = Command::new(resolve_program("sudo"));
    command.args(["-n", "true"]).stdin(Stdio::null());

    matches!(
        output_with_timeout(command, "sudo", Duration::from_secs(5)).await,
        Ok(output) if output.status.success()
    )
}

/// Run a command and kill it on timeout.
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

/// Find the absolute path of a command.
///
/// `pkexec` and `sudo` run with a sanitized environment, so absolute paths are
/// the most reliable choice.
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

/// Directory that holds the pid files.
///
/// As root this must be `/run`: that directory is owned by root with mode
/// `0700`, which is what makes the pid file trustworthy. An ordinary user
/// creating a group never reaches DHCP (the group owner already requires
/// root), but if it does, falling back to the temporary directory is still
/// allowed.
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

/// Ensure the runtime directory used by root belongs only to root.
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

/// Find the object path named `interface` in the `Interfaces` property.
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

/// Extract object paths from the `ao` type (an array inside `OwnedValue`).
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

/// Extract an object path from a value.
fn object_path_from_value(value: &OwnedValue) -> Option<String> {
    value
        .downcast_ref::<zbus::zvariant::ObjectPath<'_>>()
        .ok()
        .map(|path| path.as_str().to_owned())
}

/// Extract a string from a value.
fn string_from_value(value: &OwnedValue) -> Option<String> {
    value
        .downcast_ref::<zbus::zvariant::Str<'_>>()
        .ok()
        .map(|text| text.as_str().to_owned())
}

/// Extract a byte array from a value and treat it as text.
fn bytes_to_string(value: &OwnedValue) -> Option<String> {
    let array = value.downcast_ref::<zbus::zvariant::Array<'_>>().ok()?;
    let bytes = array
        .iter()
        .map(|item| item.downcast_ref::<u8>())
        .collect::<Result<Vec<_>, _>>()
        .ok()?;

    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Extract a u16 from a value.
fn u16_from_value(value: &OwnedValue) -> Option<u16> {
    value.downcast_ref::<u16>().ok()
}
