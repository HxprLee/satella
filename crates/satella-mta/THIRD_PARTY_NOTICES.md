# Third-party notices for `satella-mta`

This crate contains code forked from [Tinnci/cattysend].

| Field | Value |
| --- | --- |
| Upstream | <https://github.com/Tinnci/cattysend> |
| Upstream commit | `c0b2c757279c051c8f1ecc408462ac5a2d53dd8b` |
| Upstream license | MIT |
| Forked path | `vendor/mta-core/` |
| Upstream license text | `vendor/LICENSE.mta` |

## What was changed

* The package was renamed from `cattysend-core` to `satella-mta-vendor`, so it
  cannot collide with the upstream crate.
* The crate was detached from the upstream workspace, so it carries its own
  dependency list. The dependency versions are upstream's.
* The `cattysend-proto` crate was not forked: the core crate does not depend
  on it.
* `vendor/mta-core/src/wifi/p2p_sender.rs`: the hotspot creation now falls
  back to 2.4 GHz when the requested band cannot be activated. Some adapters
  cannot run a 5 GHz access point, and the hotspot then stays in
  `ACTIVATING` forever.
* `vendor/mta-core/src/wifi/p2p_sender.rs`: the generated SSID now has the
  `DIRECT-xx` shape the Mutual Transfer Alliance clients scan for (two hex
  digits after the prefix). The previous eight-character suffix produced
  SSIDs the clients ignored.
* `vendor/mta-core/src/wifi/p2p_sender.rs`: the MAC address sent to the peer
  is now read from the P2P interface first, as the protocol specification
  requires the peer to compare it against the group's owner address.
* `vendor/mta-core/src/wifi/nm_dbus.rs`: hotspot creation now reports the
  underlying NetworkManager error instead of a generic message.
* `vendor/mta-core/src/crypto/ble_security.rs`: `BleSecurityPersistent` can
  now be stored on disk and reloaded. The ECDH key pair and the sender ID
  have to survive a restart, otherwise the peers see a new device every time
  this one starts.
* `vendor/mta-core/src/ble/server.rs`: the GATT server now takes the sender
  ID and the advertisement random data from the persistent identity, so the
  device announces the same identity across restarts.
* `vendor/mta-core/src/workflow/receiver.rs` and
  `vendor/mta-core/src/workflow/sender.rs`: both workflows take an optional
  path for the persistent identity.
* `vendor/mta-core/src/wifi/p2p_go.rs`: the privileged transport is now behind
  the `GroupOwnerBackend` trait. `InProcessGroupOwner` keeps the original
  in-process wpa_supplicant path when Satella runs as root;
  `HelperGroupOwner` calls the narrow `satella-mta-helper` binary when it runs
  as an ordinary user. The DHCP pid file is treated as the only authority for
  which process to stop, and `/run/satella-mta` is hardened to root-only when
  the helper creates it.
* `vendor/mta-core/src/wifi/p2p_sender.rs`: selects the backend above instead
  of always talking to wpa_supplicant in-process, and cleans up through it.
* `vendor/mta-core/src/transfer/sender_server.rs`: the transfer server can
  serve `/websocket` and `/download` over TLS on a single port. The clients
  connect with `wss://` and `https://`, so plain HTTP does not work.

Apart from the changes listed above, the Rust sources under
`vendor/mta-core/src/` are upstream's, unchanged.

## License compatibility

The MIT license is compatible with the GPL-3.0-or-later license Satella is
distributed under. The upstream copyright headers and license text are
preserved.

## Dependency note

The vendored crate depends on a fork of the `bluer` crate for Bluetooth Low
Energy support:

| Field | Value |
| --- | --- |
| Fork | <https://github.com/Tinnci/bluer> |
| Branch | `feature/scan-response-data` |
| License | MIT |

That fork adds the scan response control the Mutual Transfer Alliance protocol
needs to announce a device name. It is used under the MIT license, which is
compatible with GPL-3.0.
