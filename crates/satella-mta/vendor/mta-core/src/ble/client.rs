//! BLE Client - 用于发送端连接接收端
//!
//! 提供与 CatShare GATT Server 通信的客户端实现。
//!
//! # 连接流程
//!
//! 1. 连接到目标设备的 GATT Server
//! 2. 读取 CHAR_STATUS 获取 DeviceInfo (包含对方公钥)
//! 3. 派生会话密钥 (ECDH)
//! 4. 加密 P2pInfo 并写入 CHAR_P2P
//!
//! # 安全性
//!
//! - 使用 ECDH P-256 密钥协商
//! - P2pInfo 中的敏感字段 (SSID, PSK, MAC) 使用 AES-256-CTR 加密
//! - 每次连接使用新的临时密钥对

use crate::ble::{DeviceInfo, MAIN_SERVICE_UUID, P2P_CHAR_UUID, STATUS_CHAR_UUID};
use crate::crypto::{BleSecurity, BleSecurityPersistent};
use crate::wifi::P2pInfo;
use btleplug::api::{Central, Characteristic, Manager as _, Peripheral, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral as PlatformPeripheral};
use log::{debug, info, trace, warn};
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use uuid::Uuid;

/// BLE 客户端错误
#[derive(Debug, thiserror::Error)]
pub enum BleClientError {
    #[error("No Bluetooth adapters found")]
    NoAdapter,

    #[error("Device not found")]
    DeviceNotFound,

    #[error("Service not found: {0}")]
    ServiceNotFound(Uuid),

    #[error("Characteristic not found: {0}")]
    CharacteristicNotFound(Uuid),

    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    #[error("IO error: {0}")]
    IoError(#[from] btleplug::Error),

    #[error("Protocol error: {0}")]
    ProtocolError(String),
}

pub struct BleClient {
    adapter: Adapter,
    security: Option<Arc<BleSecurityPersistent>>,
}

impl BleClient {
    pub async fn new() -> Result<Self, BleClientError> {
        let manager = Manager::new().await?;
        let adapters = manager.adapters().await?;
        let adapter = adapters
            .into_iter()
            .next()
            .ok_or(BleClientError::NoAdapter)?;

        Ok(Self {
            adapter,
            security: None,
        })
    }

    /// 设置持久化安全上下文，使发送端身份保持稳定
    pub fn with_security(mut self, security: Arc<BleSecurityPersistent>) -> Self {
        self.security = Some(security);
        self
    }

    /// 连接到设备并执行 P2P 握手
    ///
    /// 返回接收端的 DeviceInfo
    pub async fn connect_and_handshake(
        &self,
        device_address: &str,
        p2p_info: &P2pInfo,
        sender_id: &str,
    ) -> Result<DeviceInfo, BleClientError> {
        self.connect_and_handshake_named(device_address, None, p2p_info, sender_id)
            .await
    }

    /// 和 `connect_and_handshake` 一样，但允许在随机地址失效时按设备名找。
    ///
    /// Android 的 BLE 地址是会轮换的随机地址。扫描器看到的地址到真正连接
    /// 时可能已经变了；只按地址找会报 `Device not found`。
    pub async fn connect_and_handshake_named(
        &self,
        device_address: &str,
        device_name: Option<&str>,
        p2p_info: &P2pInfo,
        sender_id: &str,
    ) -> Result<DeviceInfo, BleClientError> {
        // 查找目标设备
        let peripheral = self.find_device(device_address, device_name).await?;

        // 连接。Android 刚被扫描到时偶尔还会拒绝一次连接，给几次机会。
        info!("Connecting to BLE device: {}", device_address);
        for attempt in 1..=3 {
            match peripheral.connect().await {
                Ok(()) => break,
                Err(err) if attempt < 3 => {
                    debug!("BLE connect failed (attempt {attempt}/3): {err}");
                    time::sleep(Duration::from_millis(500)).await;
                }
                Err(err) => return Err(err.into()),
            }
        }

        // 等待连接稳定。刚连上时 BlueZ 可能还没把属性读完。
        time::sleep(Duration::from_millis(800)).await;

        // 请求更大的 MTU
        // Note: btleplug 不直接支持 MTU 请求，跳过

        // 发现服务
        debug!("Discovering GATT services...");
        peripheral.discover_services().await?;

        // 查找并读取 STATUS 特征
        let status_char = self.find_characteristic(&peripheral, STATUS_CHAR_UUID)?;
        let status_data = read_with_retry(&peripheral, &status_char).await?;
        let device_info: DeviceInfo = serde_json::from_slice(&status_data)
            .map_err(|e| BleClientError::ProtocolError(format!("Invalid DeviceInfo: {}", e)))?;

        debug!(
            "Remote DeviceInfo: state={}, mac={}",
            device_info.state, device_info.mac
        );
        trace!("Full DeviceInfo: {:?}", device_info);

        // 如果对方提供了公钥，派生会话密钥并加密 P2P 信息
        let p2p_data = if let Some(peer_key) = &device_info.key {
            let (sender_public_key, cipher) = if let Some(sec) = &self.security {
                // 使用持久化上下文
                let pub_key = sec.get_public_key().to_string();
                let cip = sec.derive_session_key(peer_key).map_err(|e| {
                    BleClientError::ProtocolError(format!("Key exchange failed: {}", e))
                })?;
                (pub_key, cip)
            } else {
                // 回退到临时上下文
                let security = BleSecurity::new().map_err(|e| {
                    BleClientError::ProtocolError(format!("Failed to init security: {}", e))
                })?;
                let pub_key = security.get_public_key().to_string();
                let cip = security.derive_session_key(peer_key).map_err(|e| {
                    BleClientError::ProtocolError(format!("Key exchange failed: {}", e))
                })?;
                (pub_key, cip)
            };

            // 加密 P2P 信息
            let encrypted_p2p = P2pInfo::with_encryption(
                sender_id.to_string(),
                cipher
                    .encrypt(&p2p_info.ssid)
                    .map_err(|e| BleClientError::ProtocolError(e.to_string()))?,
                cipher
                    .encrypt(&p2p_info.psk)
                    .map_err(|e| BleClientError::ProtocolError(e.to_string()))?,
                cipher
                    .encrypt(&p2p_info.mac)
                    .map_err(|e| BleClientError::ProtocolError(e.to_string()))?,
                p2p_info.port,
                sender_public_key,
            );
            serde_json::to_vec(&encrypted_p2p)
                .map_err(|e| BleClientError::ProtocolError(e.to_string()))?
        } else {
            // 不加密
            serde_json::to_vec(p2p_info)
                .map_err(|e| BleClientError::ProtocolError(e.to_string()))?
        };

        // 写入 P2P 特征
        let p2p_char = self.find_characteristic(&peripheral, P2P_CHAR_UUID)?;
        info!(
            "Writing encrypted P2P info ({} bytes) to receiver",
            p2p_data.len()
        );
        write_with_retry(&peripheral, &p2p_char, &p2p_data).await?;

        // 断开连接
        peripheral.disconnect().await?;

        Ok(device_info)
    }

    async fn find_device(
        &self,
        address: &str,
        name: Option<&str>,
    ) -> Result<PlatformPeripheral, BleClientError> {
        if let Some(peripheral) = self.device_by_address(address).await? {
            return Ok(peripheral);
        }

        // 适配器自己的缓存里没有目标：先扫一轮。扫描器用的是另一个
        // BlueZ 会话，它看到的设备不一定在这个 `Adapter` 的列表里。
        debug!("{address} is not in the adapter cache, scanning for it");
        if let Err(err) = self.adapter.start_scan(ScanFilter::default()).await {
            debug!("could not start a BLE scan: {err}");
        }

        let deadline = time::Instant::now() + Duration::from_secs(5);
        let mut found = None;

        loop {
            if let Some(peripheral) = self.device_by_address(address).await? {
                found = Some(peripheral);
                break;
            }

            if time::Instant::now() >= deadline {
                break;
            }

            time::sleep(Duration::from_millis(250)).await;
        }

        let _ = self.adapter.stop_scan().await;

        if let Some(peripheral) = found {
            return Ok(peripheral);
        }

        // 随机地址可能已经轮换。设备名是稳定的，但只在唯一匹配时采用，
        // 免得把文件发给同名的另一台设备。
        if let Some(name) = name.filter(|name| !name.is_empty())
            && let Some(peripheral) = self.device_by_name(name).await?
        {
            let actual = peripheral
                .properties()
                .await
                .ok()
                .flatten()
                .map(|props| props.address.to_string())
                .unwrap_or_else(|| "unknown".to_owned());
            warn!("BLE address {address} rotated; using {name:?} at {actual}");
            return Ok(peripheral);
        }

        Err(BleClientError::DeviceNotFound)
    }

    async fn device_by_address(
        &self,
        address: &str,
    ) -> Result<Option<PlatformPeripheral>, BleClientError> {
        for peripheral in self.adapter.peripherals().await? {
            if let Some(props) = peripheral.properties().await?
                && props.address.to_string().eq_ignore_ascii_case(address)
            {
                return Ok(Some(peripheral));
            }
        }

        Ok(None)
    }

    async fn device_by_name(
        &self,
        name: &str,
    ) -> Result<Option<PlatformPeripheral>, BleClientError> {
        let mut matches = Vec::new();

        for peripheral in self.adapter.peripherals().await? {
            if let Some(props) = peripheral.properties().await?
                && props.local_name.as_deref() == Some(name)
            {
                matches.push(peripheral);
            }
        }

        if matches.len() == 1 {
            Ok(matches.pop())
        } else {
            Ok(None)
        }
    }

    fn find_characteristic(
        &self,
        peripheral: &PlatformPeripheral,
        uuid: Uuid,
    ) -> Result<Characteristic, BleClientError> {
        for service in peripheral.services() {
            if service.uuid == MAIN_SERVICE_UUID {
                for char in service.characteristics {
                    if char.uuid == uuid {
                        return Ok(char);
                    }
                }
            }
        }
        Err(BleClientError::CharacteristicNotFound(uuid))
    }
}

/// 读一个特征；ATT 请求偶尔会超时，给三次机会。
async fn read_with_retry(
    peripheral: &PlatformPeripheral,
    characteristic: &Characteristic,
) -> Result<Vec<u8>, BleClientError> {
    let mut last_error: Option<btleplug::Error> = None;

    for attempt in 1..=3 {
        match peripheral.read(characteristic).await {
            Ok(data) => return Ok(data),
            Err(err) => {
                debug!("BLE read failed (attempt {attempt}/3): {err}");
                last_error = Some(err);

                if attempt < 3 {
                    time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    }

    Err(last_error.expect("the loop runs at least once").into())
}

/// 写一个特征；同样的重试逻辑。写入的是同一个值，重试是幂等的。
async fn write_with_retry(
    peripheral: &PlatformPeripheral,
    characteristic: &Characteristic,
    data: &[u8],
) -> Result<(), BleClientError> {
    let mut last_error: Option<btleplug::Error> = None;

    for attempt in 1..=3 {
        match peripheral
            .write(characteristic, data, WriteType::WithResponse)
            .await
        {
            Ok(()) => return Ok(()),
            Err(err) => {
                debug!("BLE write failed (attempt {attempt}/3): {err}");
                last_error = Some(err);

                if attempt < 3 {
                    time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    }

    Err(last_error.expect("the loop runs at least once").into())
}
