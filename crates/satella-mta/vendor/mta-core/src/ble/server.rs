//! GATT Server - receiver-side BLE advertisement and service
//!
//! Provides a BLE GATT server implementation compatible with CatShare (Android).
//!
//! # Features
//!
//! - Publishes BLE advertisements (compatible with the CatShare advertisement format)
//! - Provides a GATT service containing the STATUS and P2P characteristics
//! - Handles P2P info writes from the sender
//!
//! # Advertisement data format
//!
//! A CatShare-compatible advertisement contains:
//! - Service UUID: `00003331-0000-1000-8000-008123456789`
//! - Service Data (0x01FF): 6 bytes of identity data
//! - Scan Response (0xFFFF): 27 bytes, containing the device name and protocol version

use log::{debug, error, info, trace};

use crate::ble::{
    ADV_SERVICE_UUID, DeviceInfo, MAIN_SERVICE_UUID, P2P_CHAR_UUID, STATUS_CHAR_UUID,
};
use crate::config::{AppSettings, BrandId};
use crate::crypto::BleSecurityPersistent;
use crate::wifi::P2pInfo;
use bluer::{
    adv::Advertisement,
    gatt::local::{
        Application, Characteristic, CharacteristicRead, CharacteristicWrite,
        CharacteristicWriteMethod, ReqError, Service,
    },
};
use futures_util::FutureExt;
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// Generate the sender ID from the random data
fn sender_id_from_random_data(random_data: &[u8; 2]) -> String {
    format!("{:02x}{:02x}", random_data[0], random_data[1])
}

/// P2P info receive event
#[derive(Debug, Clone)]
pub struct P2pReceiveEvent {
    pub p2p_info: P2pInfo,
    pub sender_public_key: Option<String>,
}

/// GATT Server state
pub struct GattServerState {
    pub device_info: DeviceInfo,
    pub device_info_bytes: Vec<u8>,
}

impl GattServerState {
    pub fn new(mac_address: String, public_key: String) -> anyhow::Result<Self> {
        let device_info = DeviceInfo::new(public_key, mac_address);
        let device_info_bytes = serde_json::to_vec(&device_info)?;

        Ok(Self {
            device_info,
            device_info_bytes,
        })
    }

    pub fn update_mac(&mut self, mac: String) -> anyhow::Result<()> {
        self.device_info.mac = mac;
        self.device_info_bytes = serde_json::to_vec(&self.device_info)?;
        Ok(())
    }
}

/// GATT Server
pub struct GattServer {
    state: Arc<Mutex<GattServerState>>,
    p2p_tx: mpsc::Sender<P2pReceiveEvent>,
    p2p_rx: Option<mpsc::Receiver<P2pReceiveEvent>>,
    /// Random data (2 bytes), used for the sender ID and advertisement identity
    random_data: [u8; 2],
    sender_id: String,
    device_name: String,
    security: Option<Arc<BleSecurityPersistent>>,
    /// Vendor ID
    brand_id: BrandId,
    /// Whether 5GHz is supported
    supports_5ghz: bool,
}

impl GattServer {
    /// Create a new GATT Server
    pub fn new(
        mac_address: String,
        device_name: String,
        public_key: String,
    ) -> anyhow::Result<Self> {
        let state = GattServerState::new(mac_address, public_key)?;

        let (p2p_tx, p2p_rx) = mpsc::channel(16);
        // Generate random data (2 bytes) that stays constant for the whole GATT Server lifetime
        let random_data: [u8; 2] = rand::random();
        let sender_id = sender_id_from_random_data(&random_data);

        Ok(Self {
            state: Arc::new(Mutex::new(state)),
            p2p_tx,
            p2p_rx: Some(p2p_rx),
            random_data,
            sender_id,
            device_name,
            security: None,
            brand_id: BrandId::Linux,
            supports_5ghz: true,
        })
    }

    /// Create a GattServer from AppSettings
    pub fn from_settings(
        mac_address: String,
        public_key: String,
        settings: &AppSettings,
    ) -> anyhow::Result<Self> {
        let mut server = Self::new(mac_address, settings.device_name.clone(), public_key)?;
        server.brand_id = settings.brand_id;
        server.supports_5ghz = settings.supports_5ghz;
        Ok(server)
    }

    /// Set the security context, used to automatically decrypt P2P info
    pub fn with_security(mut self, security: Arc<BleSecurityPersistent>) -> Self {
        // The identity in the advertisement (random data and sender ID) must stay unchanged
        // across restarts, just like the key pair; otherwise the peer treats this device as
        // a new one.
        let sender_id = security.sender_id().to_owned();
        if let Ok(bytes) = hex::decode(&sender_id)
            && bytes.len() == 2
        {
            self.random_data = [bytes[0], bytes[1]];
            self.sender_id = sender_id;
        }
        self.security = Some(security);
        self
    }

    /// Set the vendor ID
    pub fn with_brand(mut self, brand_id: BrandId) -> Self {
        self.brand_id = brand_id;
        self
    }

    /// Set 5GHz support
    pub fn with_5ghz_support(mut self, supports_5ghz: bool) -> Self {
        self.supports_5ghz = supports_5ghz;
        self
    }

    /// Get the sender ID
    pub fn sender_id(&self) -> &str {
        &self.sender_id
    }

    /// Get the P2P info receive channel
    pub fn take_p2p_receiver(&mut self) -> Option<mpsc::Receiver<P2pReceiveEvent>> {
        self.p2p_rx.take()
    }

    /// Start the GATT service
    pub async fn start(&self) -> anyhow::Result<GattServerHandle> {
        debug!("Initializing BLE session...");
        let session = bluer::Session::new().await?;

        debug!("Getting default adapter...");
        let adapter = session.default_adapter().await?;

        let adapter_name = adapter.name().to_string();
        debug!("Powering on adapter: {}", adapter_name);
        adapter.set_powered(true).await?;

        // Pairing is not part of the MTA protocol, but do **not** touch the adapter's Pairable
        // setting here.
        //
        // The adapter is shared between sending and receiving: the sender is also a GATT
        // client using the same adapter. Turning Pairable off makes the kernel reject pairing
        // requests outright, and the phone (CatShare) initiates SMP itself when establishing
        // the link — so BlueZ waits for a pairing that never completes, `ServicesResolved`
        // never arrives, and btleplug reports `Service discovery timed out` after 5 seconds.
        // Sending then fails completely.
        //
        // To get "no pairing dialog", **do not register an agent** (with no agent BlueZ
        // cannot find one and rejects outright), rather than turning off the adapter.
        let state = self.state.clone();
        let p2p_tx = self.p2p_tx.clone();

        // STATUS characteristic - read-only, returns DeviceInfo JSON
        let state_for_read = state.clone();
        let status_char = Characteristic {
            uuid: STATUS_CHAR_UUID,
            read: Some(CharacteristicRead {
                read: true,
                fun: Box::new(move |req| {
                    let state = state_for_read.clone();
                    async move {
                        let s = state.lock().await;
                        let offset = req.offset as usize;
                        debug!(
                            "STATUS characteristic read: offset={}, data_len={}",
                            offset,
                            s.device_info_bytes.len()
                        );
                        if offset >= s.device_info_bytes.len() {
                            return Ok(vec![]);
                        }
                        Ok(s.device_info_bytes[offset..].to_vec())
                    }
                    .boxed()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        // P2P characteristic - writable, receives P2pInfo JSON
        let p2p_tx_clone = p2p_tx.clone();
        let security_clone = self.security.clone();
        let p2p_char = Characteristic {
            uuid: P2P_CHAR_UUID,
            write: Some(CharacteristicWrite {
                write: true,
                write_without_response: true,
                method: CharacteristicWriteMethod::Fun(Box::new(move |data, _req| {
                    let p2p_tx = p2p_tx_clone.clone();
                    let security = security_clone.clone();
                    async move {
                        match process_p2p_write(&data, security.as_deref()) {
                            Ok(event) => {
                                let _ = p2p_tx.send(event).await;
                                Ok(())
                            }
                            Err(e) => {
                                error!("Failed to process P2P write: {}", e);
                                Err(ReqError::Failed)
                            }
                        }
                    }
                    .boxed()
                })),
                ..Default::default()
            }),
            ..Default::default()
        };

        // Create the GATT application
        let app = Application {
            services: vec![Service {
                uuid: MAIN_SERVICE_UUID,
                primary: true,
                characteristics: vec![status_char, p2p_char],
                ..Default::default()
            }],
            ..Default::default()
        };

        debug!(
            "Registering GATT application with service_uuid={}",
            MAIN_SERVICE_UUID
        );
        let _app_handle = adapter.serve_gatt_application(app).await?;
        debug!("GATT application registered successfully");

        // Build the Legacy BLE advertisement
        // Key: secondary_channel: None forces Legacy Advertising PDUs
        let random_data = self.random_data;

        let mut service_uuids = BTreeSet::new();
        service_uuids.insert(ADV_SERVICE_UUID);

        // ========== Main advertisement packet data (31 bytes max) ==========
        // Build the identity data (Service Data, about 10 bytes)
        let flag_5ghz: u8 = if self.supports_5ghz { 0x01 } else { 0x00 };
        let brand = self.brand_id.id();
        let capability_short = ((flag_5ghz as u16) << 8) | (brand as u16);
        let ident_uuid = uuid::Uuid::from_u128(
            ((capability_short as u128) << 96) | 0x0000_1000_8000_0080_5f9b_34fb_u128,
        );

        let mut ident_payload = vec![0u8; 6];
        ident_payload[0] = random_data[0];
        ident_payload[1] = random_data[1];

        let mut service_data = std::collections::BTreeMap::new();
        service_data.insert(ident_uuid, ident_payload);

        // ========== Scan response packet data (31 bytes max) ==========
        // Build the Name Service Data (27 bytes)
        // CatShare format:
        //   Byte 0-7:   protocol header (fixed at 0)
        //   Byte 8-9:   Sender ID (same as random_data)
        //   Byte 10-25: device name (UTF-8, at most 16 bytes, null padded)
        //   Byte 26:    protocol trailer (0)
        let mut name_payload = vec![0u8; 27];
        // Set the Sender ID (bytes 8-9)
        name_payload[8] = random_data[0];
        name_payload[9] = random_data[1];
        // Set the device name (bytes 10-25, at most 16 bytes)
        let name_bytes = self.device_name.as_bytes();
        let name_len = name_bytes.len().min(16);
        name_payload[10..10 + name_len].copy_from_slice(&name_bytes[..name_len]);
        // If the name was truncated, add a tab character marker (CatShare shows "...")
        if name_bytes.len() > 16 {
            name_payload[25] = b'\t';
        }

        // Name Service Data uses UUID 0xFFFF (standard Bluetooth base)
        let name_uuid = uuid::Uuid::from_u128(0x0000_ffff_0000_1000_8000_0080_5f9b_34fb_u128);
        let mut scan_response_service_data = std::collections::BTreeMap::new();
        scan_response_service_data.insert(name_uuid, name_payload);

        let adv = Advertisement {
            advertisement_type: bluer::adv::Type::Peripheral,
            service_uuids,
            service_data,
            // ⭐ Use scan_response_service_data instead of local_name
            // This requires the BlueZ experimental feature (Experimental = true in /etc/bluetooth/main.conf)
            scan_response_service_data,
            // local_name is no longer used because CatShare does not read it
            // local_name: Some(self.device_name.clone()),
            discoverable: Some(true),
            // Key: secondary_channel: None forces Legacy Advertising
            // Not setting a secondary channel = using the primary channel = Legacy PDUs
            secondary_channel: None,
            ..Default::default()
        };

        debug!(
            "Starting Legacy BLE advertisement: service={}, ident=0x{:04x}, name='{}'",
            ADV_SERVICE_UUID, capability_short, self.device_name
        );
        let adv_handle = adapter.advertise(adv).await?;
        debug!("Legacy BLE advertisement started successfully");

        info!(
            "GATT Server started, sender_id={}, device_name='{}'",
            self.sender_id, self.device_name
        );

        Ok(GattServerHandle {
            _adv_handle: adv_handle,
            _app_handle,
            _session: session,
        })
    }
}

/// Handle a P2P characteristic write
///
/// If `security` is provided and the P2pInfo contains the sender's public key (the `key`
/// field), the SSID/PSK/MAC fields are decrypted automatically.
fn process_p2p_write(
    data: &[u8],
    security: Option<&BleSecurityPersistent>,
) -> anyhow::Result<P2pReceiveEvent> {
    let json_str = std::str::from_utf8(data)?;
    let mut p2p_info: P2pInfo = serde_json::from_str(json_str)?;

    let is_encrypted = p2p_info.key.is_some();
    let sender_public_key = p2p_info.key.clone();

    if let (Some(sender_key), Some(sec)) = (&sender_public_key, security) {
        debug!("Sender provided public key, decrypting P2P info...");
        match sec.derive_session_key(sender_key) {
            Ok(cipher) => {
                p2p_info.ssid = cipher.decrypt(&p2p_info.ssid).unwrap_or(p2p_info.ssid);
                p2p_info.psk = cipher.decrypt(&p2p_info.psk).unwrap_or(p2p_info.psk);
                p2p_info.mac = cipher.decrypt(&p2p_info.mac).unwrap_or(p2p_info.mac);
                p2p_info.key = None; // marks it as decrypted
                info!("Successfully decrypted P2P info");
            }
            Err(e) => {
                error!("Failed to derive session key: {}", e);
            }
        }
    }

    info!(
        "Received P2P info from sender, ssid='{}', port={}, decrypted={}",
        p2p_info.ssid,
        p2p_info.port,
        is_encrypted && p2p_info.key.is_none()
    );
    trace!("Full P2P info: {:?}", p2p_info);

    Ok(P2pReceiveEvent {
        p2p_info,
        sender_public_key,
    })
}

/// GATT Server Handle - keeps the service running
pub struct GattServerHandle {
    _adv_handle: bluer::adv::AdvertisementHandle,
    _app_handle: bluer::gatt::local::ApplicationHandle,
    _session: bluer::Session,
}

impl GattServerHandle {
    /// Wait for a service shutdown signal
    pub async fn wait_for_shutdown(&self) {
        // Wait forever, until dropped
        std::future::pending::<()>().await;
    }
}
