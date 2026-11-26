//! BLE link security: keep pairing from happening and hold the ATT single-packet write path.
//!
//! # Why this module exists
//!
//! The CatShare / MTA protocol does not need BLE pairing: SSID/PSK/MAC are protected
//! by application-layer ECDH + AES-CTR (see [`crate::crypto::ble_security`]); the link
//! itself is plaintext and unbound.
//!
//! But the phone (CatShare) initiates SMP itself when establishing the link, so a pairing
//! dialog pops up on the phone. After receiving `INSUFFICIENT_ENCRYPTION`, BlueZ's ATT
//! layer also **automatically** raises the channel security level to
//! `BT_ATT_SECURITY_MEDIUM` (`change_security()` in `src/shared/att.c`), and a MEDIUM
//! level link can only be obtained through SMP pairing.
//!
//! Neither of the two ways to avoid pairing may turn off the adapter's `Pairable`:
//!
//! 1. **Do not register a pairing agent**: with no agent, BlueZ cannot find one and
//!    rejects pairing requests outright, so negotiation never advances far enough to
//!    show a pairing dialog on the peer. The adapter stays pairable, so SMP initiated
//!    by the phone does not stall the link.
//! 2. **Keep the P2P write on ATT's single-packet path**: the payload must fit in one
//!    ATT packet; otherwise BlueZ uses long-write (prepare-write/execute-write), and
//!    that is exactly where the phone most often replies `INSUFFICIENT_ENCRYPTION`.
//!
//! # Single-packet writes
//!
//! ATT's payload limit is `MTU - 3` (3 bytes is the ATT header). The default LE MTU is 23,
//! so only 20 bytes fit; the encrypted P2P JSON is about 266 bytes and does not fit. BlueZ
//! negotiates a larger MTU (`[GATT] ExchangeMTU`, default 517), but **BlueZ does the
//! negotiation itself**, and only once, when the GATT client is initialized
//! (`gatt_client_init()`). If that negotiation does not happen in time, the MTU stays 23
//! and the write necessarily falls back to long-write.
//!
//! Measured on the phone side, the reported ATT MTU is 517, so the encrypted payload fits
//! in one packet.

use std::time::Duration;

use log::{debug, warn};

use crate::ble::{MAIN_SERVICE_UUID, P2P_CHAR_UUID};

/// ATT header length: 1-byte opcode + 2-byte handle.
pub const ATT_HEADER_LEN: usize = 3;

/// bluer's `mtu_workaround()`: it under-reports the MTU by this many bytes when reading.
pub const BLUER_MTU_WORKAROUND: u16 = 5;

/// Default LE ATT MTU (when not negotiated).
pub const DEFAULT_LE_MTU: u16 = 23;

/// Upper bound on waiting for MTU negotiation to complete.
pub const MTU_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// The MTU needed for the payload to fit in a single ATT packet.
///
/// This is how BlueZ decides in `src/gatt-client.c`:
/// `if (value_len <= mtu - 3 && !offset)` uses a single-packet write, otherwise long-write.
pub fn mtu_for_payload(payload_len: usize) -> u16 {
    let needed = payload_len + ATT_HEADER_LEN;
    if needed <= DEFAULT_LE_MTU as usize {
        DEFAULT_LE_MTU
    } else {
        needed as u16
    }
}

/// Whether the payload fits in an ATT packet with MTU `mtu`.
pub fn fits_in_single_packet(payload_len: usize, mtu: u16) -> bool {
    payload_len + ATT_HEADER_LEN <= mtu as usize
}

/// Wait until the P2P characteristic's ATT MTU can hold `payload_len`, or until timeout.
///
/// On timeout, returns the last MTU read (which may still be insufficient); the caller
/// decides whether to fail hard.
///
/// # Where the MTU comes from
///
/// `btleplug` does not expose the MTU, so this uses a parallel `bluer` session to read
/// BlueZ's `org.bluez.GattCharacteristic1.MTU`. That property only becomes larger than
/// the default 23 after `ExchangeMTU` completes.
///
/// bluer's getter subtracts another 5 (`mtu_workaround()`; BlueZ drops data at the end of
/// the packet), so 5 is added back here to get an ATT MTU that can be compared directly
/// against BlueZ's `value_len <= mtu - 3` rule.
///
/// Returning `None` means the MTU was **not read** (the property is unavailable, the bluer
/// session could not be opened, or MTU negotiation has not happened yet). That differs from
/// "it was read and the MTU really is 23": the former must not fail the send; it only means
/// we cannot tell whether the payload fits.
pub async fn wait_for_single_packet_mtu(
    device_address: &str,
    payload_len: usize,
) -> anyhow::Result<Option<u16>> {
    let needed = mtu_for_payload(payload_len);
    let deadline = tokio::time::Instant::now() + MTU_WAIT_TIMEOUT;
    let mut last_mtu: Option<u16> = None;

    // One bluer session, reusing the D-Bus connection; the MTU is a BlueZ-side property, so
    // reading it again does not trigger a new negotiation.
    let session = match bluer::Session::new().await {
        Ok(session) => Some(session),
        Err(err) => {
            debug!("Could not open a BlueZ session for the MTU probe: {err}");
            None
        }
    };
    let adapter = match &session {
        Some(session) => session.default_adapter().await.ok(),
        None => None,
    };
    let address: Option<bluer::Address> = match device_address.parse() {
        Ok(address) => Some(address),
        Err(err) => {
            debug!("Invalid BLE address {device_address}: {err}");
            None
        }
    };
    let device = match (adapter, address) {
        (Some(adapter), Some(address)) => adapter.device(address).ok(),
        _ => None,
    };

    loop {
        if let Some(device) = &device {
            match read_mtu_from(device).await {
                Ok(mtu) => {
                    last_mtu = Some(mtu);
                    debug!(
                        "ATT MTU on {device_address} is {mtu}; need {needed} for a {payload_len}-byte P2P write"
                    );
                    if fits_in_single_packet(payload_len, mtu) {
                        return Ok(Some(mtu));
                    }
                }
                Err(err) => {
                    debug!("Could not read ATT MTU on {device_address} yet: {err}");
                }
            }
        }

        if tokio::time::Instant::now() >= deadline {
            return Ok(last_mtu);
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Read the ATT MTU of the P2P characteristic from a connected device.
async fn read_mtu_from(device: &bluer::Device) -> anyhow::Result<u16> {
    for service in device.services().await? {
        if service.uuid().await? != MAIN_SERVICE_UUID {
            continue;
        }
        for characteristic in service.characteristics().await? {
            if characteristic.uuid().await? != P2P_CHAR_UUID {
                continue;
            }
            let mtu = characteristic.mtu().await? as u16 + BLUER_MTU_WORKAROUND;
            return Ok(mtu.max(DEFAULT_LE_MTU));
        }
    }

    Err(anyhow::anyhow!(
        "P2P characteristic {P2P_CHAR_UUID} not found"
    ))
}

/// Take the adapter out of its "pairable" state.
///
/// The adapter is normally `Pairable = true` (the BlueZ default). That alone does not cause
/// pairing, but if something requests pairing, BlueZ complies. Turning `Pairable` off lets
/// the kernel reject pairing requests, so pairing never even starts.
///
/// **Do not use this in MTA.** The adapter is shared between sending and receiving: the
/// sender also uses the same adapter as a GATT client. Turning Pairable off makes the kernel
/// reject SMP initiated by the phone (CatShare); BlueZ then waits for a pairing that never
/// completes, `ServicesResolved` never arrives, and btleplug reports
/// `Service discovery timed out` after 5 seconds, so sending fails completely.
///
/// To get "no pairing dialog on the desktop", **do not register a pairing agent**: with no
/// agent BlueZ cannot find one and rejects pairing outright, instead of advancing
/// negotiation until the peer shows a pairing dialog.
#[allow(dead_code)]
pub async fn make_adapter_unpairable(adapter: &bluer::Adapter) -> anyhow::Result<()> {
    if let Err(err) = adapter.set_pairable(false).await {
        warn!("Could not turn the adapter unpairable: {err}");
        return Err(err.into());
    }
    debug!("The adapter no longer accepts pairing requests");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encrypted P2P JSON does not fit in the default LE MTU, so a larger MTU must be
    /// negotiated.
    #[test]
    fn the_encrypted_payload_needs_more_than_the_default_mtu() {
        // The actual payload is about 266 bytes (SSID/PSK/MAC are 24-32 bytes each after
        // encryption, plus the base64 of the 91-byte SPKI public key).
        let payload = 266;
        assert!(!fits_in_single_packet(payload, DEFAULT_LE_MTU));
        assert!(fits_in_single_packet(payload, mtu_for_payload(payload)));
    }

    /// The needed MTU is exactly enough to hold the payload.
    #[test]
    fn the_needed_mtu_holds_the_payload() {
        for payload in [0usize, 1, 20, 21, 100, 266, 512] {
            let mtu = mtu_for_payload(payload);
            assert!(
                fits_in_single_packet(payload, mtu),
                "payload {payload} does not fit in MTU {mtu}"
            );
        }
    }

    /// For an empty payload, the default MTU is enough.
    #[test]
    fn an_empty_payload_fits_the_default_mtu() {
        assert_eq!(mtu_for_payload(0), DEFAULT_LE_MTU);
        assert!(fits_in_single_packet(0, DEFAULT_LE_MTU));
    }

    /// bluer under-reports the MTU by 5 bytes; adding them back gives the real ATT MTU.
    ///
    /// Before negotiation BlueZ reports 23 and bluer reports 18. Without adding those 5
    /// bytes, a legal 20-byte single-packet write would be misjudged as needing long-write.
    #[test]
    fn the_bluer_workaround_is_added_back() {
        let bluez_mtu = 23u16;
        let reported_by_bluer = bluez_mtu - BLUER_MTU_WORKAROUND;
        let recovered = reported_by_bluer + BLUER_MTU_WORKAROUND;
        assert_eq!(recovered, bluez_mtu);
        // Under the default MTU, a 20-byte ATT payload fits exactly.
        assert!(fits_in_single_packet(20, recovered));
        // 21 bytes no longer fit.
        assert!(!fits_in_single_packet(21, recovered));
    }
}
