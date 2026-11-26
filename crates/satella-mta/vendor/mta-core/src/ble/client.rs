//! BLE Client - lets the sender connect to the receiver
//!
//! Provides a client implementation for communicating with the CatShare GATT Server.
//!
//! # Connection flow
//!
//! 1. Connect to the target device's GATT Server
//! 2. Read CHAR_STATUS to get the DeviceInfo (including the peer's public key)
//! 3. Derive the session key (ECDH)
//! 4. Encrypt P2pInfo and write it to CHAR_P2P
//!
//! # Security
//!
//! - Uses ECDH P-256 key agreement
//! - Sensitive fields in P2pInfo (SSID, PSK, MAC) are encrypted with AES-256-CTR
//! - Each connection uses a new ephemeral key pair

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

/// BLE client errors
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

    /// The peer or BlueZ pushed the link into a state that requires SMP pairing. MTA does not need pairing.
    ///
    /// Common sources: after ATT `INSUFFICIENT_ENCRYPTION`, BlueZ raises the level to MEDIUM and retries;
    /// or SMP gets stuck during the connection, so BlueZ never sees `ServicesResolved` and
    /// bluez-async reports `Service discovery timed out` after its hard-coded 5 seconds.
    ///
    /// Note the wording: a timeout alone **cannot** prove the peer wants encryption. The MTA
    /// characteristics on the phone side measured only `read`/`write` flags and MTU 517, and
    /// never required encryption.
    /// The usual real cause of the timeout is that the adapter was made unpairable, so the
    /// kernel rejected the phone's SMP outright and pairing can never complete.
    #[error(
        "the peer started a pairing handshake that never completed, which MTA does not need: {0}"
    )]
    PairingRequired(String),

    /// The ATT MTU is too small, so the P2P write uses long-write, which in turn triggers a pairing upgrade.
    #[error("ATT MTU {mtu} is too small for a {payload_len}-byte P2P write (need {needed})")]
    AttMtuTooSmall {
        mtu: u16,
        payload_len: usize,
        needed: u16,
    },
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

        // The sender does not register an agent that rejects pairing: once SMP is triggered,
        // rejecting it only makes ServicesResolved never arrive, which shows up as Service
        // discovery timed out. The right fix is a single-packet MTU write that avoids
        // triggering SMP (see connect_and_handshake).

        Ok(Self {
            adapter,
            security: None,
        })
    }

    /// Set a persistent security context so the sender identity stays stable
    pub fn with_security(mut self, security: Arc<BleSecurityPersistent>) -> Self {
        self.security = Some(security);
        self
    }

    /// Connect to a device and perform the P2P handshake
    ///
    /// Returns the receiver's DeviceInfo
    pub async fn connect_and_handshake(
        &self,
        device_address: &str,
        p2p_info: &P2pInfo,
        sender_id: &str,
    ) -> Result<DeviceInfo, BleClientError> {
        self.connect_and_handshake_named(device_address, None, p2p_info, sender_id)
            .await
    }

    /// Same as `connect_and_handshake`, but allows falling back to the device name when
    /// the random address is stale.
    ///
    /// Android BLE addresses are random addresses that rotate. The address the scanner saw
    /// may already have changed by the time the real connection happens; looking it up by
    /// address alone reports `Device not found`.
    pub async fn connect_and_handshake_named(
        &self,
        device_address: &str,
        device_name: Option<&str>,
        p2p_info: &P2pInfo,
        sender_id: &str,
    ) -> Result<DeviceInfo, BleClientError> {
        // Find the target device
        let peripheral = self.find_device(device_address, device_name).await?;

        // Connect. An Android device that was just discovered sometimes rejects a connection
        // once, so give it a few chances. bluez-async's connect() waits for ServicesResolved;
        // if the peer is pushing SMP, this fails with Service discovery timed out — treat
        // that as PairingRequired.
        info!("Connecting to BLE device: {}", device_address);
        for attempt in 1..=3 {
            match peripheral.connect().await {
                Ok(()) => break,
                Err(err) => {
                    let mapped = map_connect_error(err);
                    if attempt < 3 && !matches!(mapped, BleClientError::PairingRequired(_)) {
                        debug!("BLE connect failed (attempt {attempt}/3): {mapped}");
                        time::sleep(Duration::from_millis(500)).await;
                        continue;
                    }
                    return Err(mapped);
                }
            }
        }

        // Wait for the connection to settle. Right after connecting, BlueZ may not have
        // finished reading properties / negotiating the MTU.
        time::sleep(Duration::from_millis(800)).await;

        // Discover services
        debug!("Discovering GATT services...");
        peripheral
            .discover_services()
            .await
            .map_err(map_connect_error)?;

        // Find and read the STATUS characteristic
        let status_char = self.find_characteristic(&peripheral, STATUS_CHAR_UUID)?;
        let status_data = read_with_retry(&peripheral, &status_char).await?;
        let device_info: DeviceInfo = serde_json::from_slice(&status_data)
            .map_err(|e| BleClientError::ProtocolError(format!("Invalid DeviceInfo: {}", e)))?;

        debug!(
            "Remote DeviceInfo: state={}, mac={}",
            device_info.state, device_info.mac
        );
        trace!("Full DeviceInfo: {:?}", device_info);

        // If the peer provided a public key, derive the session key and encrypt the P2P info
        let p2p_data = if let Some(peer_key) = &device_info.key {
            let (sender_public_key, cipher) = if let Some(sec) = &self.security {
                // Use the persistent context
                let pub_key = sec.get_public_key().to_string();
                let cip = sec.derive_session_key(peer_key).map_err(|e| {
                    BleClientError::ProtocolError(format!("Key exchange failed: {}", e))
                })?;
                (pub_key, cip)
            } else {
                // Fall back to an ephemeral context
                let security = BleSecurity::new().map_err(|e| {
                    BleClientError::ProtocolError(format!("Failed to init security: {}", e))
                })?;
                let pub_key = security.get_public_key().to_string();
                let cip = security.derive_session_key(peer_key).map_err(|e| {
                    BleClientError::ProtocolError(format!("Key exchange failed: {}", e))
                })?;
                (pub_key, cip)
            };

            // Encrypt the P2P info
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
            // No encryption
            serde_json::to_vec(p2p_info)
                .map_err(|e| BleClientError::ProtocolError(e.to_string()))?
        };

        // Write the P2P characteristic
        let p2p_char = self.find_characteristic(&peripheral, P2P_CHAR_UUID)?;
        info!(
            "Writing encrypted P2P info ({} bytes) to receiver",
            p2p_data.len()
        );

        // The payload must fit in a single ATT packet, otherwise BlueZ uses long-write
        // (prepare-write/execute-write). That is exactly where the phone most often replies
        // `INSUFFICIENT_ENCRYPTION`, and it is also where the pairing dialog comes from.
        //
        // On the phone side the ATT MTU measures 517 and the encrypted P2P JSON is about
        // 266 bytes, so it fits in one packet. Reading the MTU is only to fail early if it
        // really does not fit, instead of discovering that at long-write time. And do not
        // force the write when it does not fit — that is precisely what triggers pairing.
        //
        // Failing to read the MTU (`None`) is not a failure: it only means we cannot tell,
        // so take BlueZ's normal write path and let it negotiate the MTU itself.
        let needed_mtu = crate::ble::security::mtu_for_payload(p2p_data.len());
        let actual_address = peripheral
            .properties()
            .await
            .ok()
            .flatten()
            .map(|props| props.address.to_string())
            .unwrap_or_else(|| device_address.to_owned());
        match crate::ble::security::wait_for_single_packet_mtu(&actual_address, p2p_data.len())
            .await
        {
            Ok(Some(gatt_mtu)) => {
                debug!(
                    "The P2P payload needs an ATT MTU of {needed_mtu}; BlueZ reports {gatt_mtu}"
                );
                if !crate::ble::security::fits_in_single_packet(p2p_data.len(), gatt_mtu) {
                    return Err(BleClientError::AttMtuTooSmall {
                        mtu: gatt_mtu,
                        payload_len: p2p_data.len(),
                        needed: needed_mtu,
                    });
                }
            }
            Ok(None) => {
                debug!("Could not read the ATT MTU; leaving the write to BlueZ's own negotiation");
            }
            Err(err) => {
                debug!("The ATT MTU probe failed: {err}");
            }
        }

        write_with_retry(&peripheral, &p2p_char, &p2p_data).await?;

        // Disconnect
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

        // The target is not in the adapter's own cache: scan first. The scanner uses another
        // BlueZ session, so devices it sees are not necessarily in this `Adapter`'s list.
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

        // The random address may have rotated. The device name is stable, but only adopt it
        // on a unique match, so files are not sent to another device with the same name.
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

/// Read a characteristic; ATT requests occasionally time out, so allow three attempts.
///
/// But **security errors are not retried**: after `INSUFFICIENT_ENCRYPTION`, BlueZ
/// automatically raises the channel security level to MEDIUM and retries, and a
/// MEDIUM-level link can only be obtained through pairing. So every retry drives the
/// pairing request again, and a pairing-code dialog may pop up on the desktop. Such
/// errors should fail immediately so the caller sees the real cause.
async fn read_with_retry(
    peripheral: &PlatformPeripheral,
    characteristic: &Characteristic,
) -> Result<Vec<u8>, BleClientError> {
    let mut last_error: Option<btleplug::Error> = None;

    for attempt in 1..=3 {
        match peripheral.read(characteristic).await {
            Ok(data) => return Ok(data),
            Err(err) => {
                if is_security_error(&err) {
                    return Err(BleClientError::PairingRequired(err.to_string()));
                }

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

/// Write a characteristic; same retry logic. It writes the same value, so retries are
/// idempotent.
///
/// Security errors are likewise not retried; see [`read_with_retry`] for the reason.
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
                if is_security_error(&err) {
                    return Err(BleClientError::PairingRequired(err.to_string()));
                }

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

/// Is a connection / service discovery failure actually stuck pairing?
///
/// Note: a service discovery timeout does **not** mean "the peer wants encryption".
/// bluez-async only allows 5 seconds for `ServicesResolved`; an unpairable adapter or
/// incomplete peer SMP both cause a timeout.
fn map_connect_error(err: btleplug::Error) -> BleClientError {
    if is_security_error(&err) || is_service_discovery_timeout(&err) {
        BleClientError::PairingRequired(err.to_string())
    } else {
        BleClientError::IoError(err)
    }
}

fn is_service_discovery_timeout(err: &btleplug::Error) -> bool {
    let haystack = format!("{} {:?}", err, err).to_lowercase();
    let normalized: String = haystack.chars().filter(|c| c.is_alphanumeric()).collect();
    normalized.contains("servicediscoverytimedout")
}

/// Is this error a signal that "the peer requires encryption"?
///
/// BlueZ translates ATT's `INSUFFICIENT_ENCRYPTION`, `AUTHENTICATION` and
/// `INSUFFICIENT_ENCRYPTION_KEY_SIZE` into the D-Bus errors
/// `org.bluez.Error.NotPermitted` / `org.bluez.Error.AuthenticationFailed`, with the
/// message "Not paired". btleplug passes D-Bus errors through unchanged.
///
/// They are recognised so that such errors are **not retried**: a retry only triggers
/// pairing again.
///
/// The catch is that `dbus::Error`'s `Display` prints only the message and **not** the
/// D-Bus error name (see the `dbus` crate's `impl Display for Error`), so both spellings
/// must be guarded: "Not permitted" (message) and "NotPermitted" (error name, only in
/// `Debug`). After normalization, both are matched in space-free form.
fn is_security_error(err: &btleplug::Error) -> bool {
    // `Display` gives the message, `Debug` gives the D-Bus error name; check both.
    let haystack = format!("{} {:?}", err, err).to_lowercase();

    let normalized: String = haystack.chars().filter(|c| c.is_alphanumeric()).collect();

    [
        "notpaired",
        "notpermitted",
        "insufficientencryption",
        "authenticationfailed",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// BlueZ translates "the peer requires encryption" into "Not paired" / "Not permitted".
    /// Such errors must not be retried: a retry only drives pairing once more.
    #[test]
    fn security_errors_are_recognised() {
        for msg in [
            "Not paired",
            "not permitted",
            "Insufficient encryption",
            "Authentication failed",
            // D-Bus error names only appear in Debug
            "org.bluez.Error.NotPermitted",
            "org.bluez.Error.AuthenticationFailed",
            "org.bluez.Error.InsufficientEncryption",
        ] {
            let err = btleplug::Error::Other(msg.into());
            assert!(
                is_security_error(&err),
                "should have been recognised as a security error: {msg}"
            );
        }
    }

    /// Ordinary errors such as timeouts are retried as usual.
    #[test]
    fn ordinary_errors_are_not_security_errors() {
        for msg in [
            "Timeout",
            "Device not found",
            "Connection refused",
            "Not connected",
            "Timed out after 5s",
        ] {
            let err = btleplug::Error::Other(msg.into());
            assert!(
                !is_security_error(&err),
                "should not have been recognised as a security error: {msg}"
            );
        }
    }

    /// bluez-async reports Service discovery timed out when SMP gets stuck.
    #[test]
    fn service_discovery_timeout_maps_to_pairing_required() {
        let err = btleplug::Error::Other("Service discovery timed out".into());
        assert!(is_service_discovery_timeout(&err));
        assert!(matches!(
            map_connect_error(err),
            BleClientError::PairingRequired(_)
        ));
    }
}
