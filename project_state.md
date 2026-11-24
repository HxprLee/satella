# Satella — State of the Project

**Stage:** v0.1.0, greenfield init complete and verified, committed.
**Last live verification:** 2026-11-23 — MTA send and receive against a OnePlus 13.

---

## Layout

| Crate | Role | Lines (src) |
|---|---|---|
| `satella-core` | Protocol-neutral types + the `ProtocolAdapter` trait. No I/O, no protocol logic. | 399 |
| `satella-runtime` | Runs adapters concurrently, merges events, routes sends by protocol. | 182 |
| `satella-localsend` | LocalSend v2 adapter (vendored engine, 560K). | 922 |
| `satella-quickshare` | Quick Share adapter (vendored engine, 336K). | 370 |
| `satella-mta` | MTA adapter (vendored engine, 244K). | 395 |
| `satella-api` | The public UniFFI surface (`satella.udl`). | 477 |
| `satella-cli` | Terminal harness: discover / send / receive. | 344 |

The three vendored crates are excluded from the workspace so upstream's own warnings don't pollute Satella's gates.

---

## Verification
```
cargo fmt --all -- --check                              ✅
cargo clippy --workspace --all-targets -- -D warnings   ✅
cargo test --workspace                                  ✅
cargo doc --workspace --no-deps                         ✅
```

**Runnable checks left behind:**
- `satella-runtime/tests/merged_discovery.rs` (3 tests) — merged list covers every protocol with labels; pending receive names the sender's protocol; send reaches the right adapter.
- `satella-localsend/tests/discovery.rs` (1 test) — two *real* LocalSend adapters finding each other via actual multicast + HTTP register. Not a stub.
- `satella-localsend/tests/identity.rs` (5 tests) — a certificate survives a restart, a corrupt file is replaced, two instances get distinct identities.
- `satella-api/tests/identity.rs` (1 test) — `Engine::with_storage_dir` writes the certificate where the platform asked.
- Vendored upstream suites also run: **99 + 35 + 13 + 5** tests, all green.
- `crates/satella-mta/vendor/mta-core/tests/p2p_go_probe.rs` — 3 **ignored** root/hardware probes. They are manual validation, not CI; see the runbook below.

**Live evidence on this machine:**
- Real file transfer end to end — `hello.txt` sent, received, content verified, receiver reporting `incoming transfer over LocalSend`.
- Two instances discovering each other, both listed.
- A real BLE MTA device (`4axI`) discovered alongside LocalSend peers in the same merged list.
- Real MTA receive end to end against a OnePlus 13: BLE handshake, join `DIRECT-…`, DHCP, WSS, download, save.
- Real MTA send end to end against the same phone: wpa_supplicant group owner on 5 GHz (`DIRECT-gu`), DHCP on 192.168.49.0/24, BLE `P2pInfo`, phone joined, WSS `sendRequest`, ZIP download, status OK. Group, DHCP and address cleaned up afterwards.
- Repeated sends: 2 files and 1 file, both OK (`/tmp/opencode/send-go-5.log`, `/tmp/opencode/send-go-6.log`; `/tmp` is ephemeral).
- Forced-failure run (ghost BLE peer): group, DHCP and address all cleaned up. Probe `sender_cleans_up_when_ble_fails` asserts this.

**Release artifacts built:** `libsatella.so` (cdylib UniFFI loads), `libsatella.a`, `libsatella.rlib`, plus generated `satella.uniffi.rs` scaffolding.

---

## MTA send path (v0.1.0)

Android CatShare/MTA receivers do **not** join a normal access point. They call
Wi-Fi Direct `p2p_connect` for a `DIRECT-xx` group, then use the group owner at
`192.168.49.1` as gateway. The sender therefore has to be a real P2P GO.

Sequence:

1. `MtaAdapter::send` (`crates/satella-mta/src/lib.rs`) resolves the peer the
   scanner saw and starts the vendor `Sender` with the persistent identity.
2. `TransferServer::start_tls()` binds `0.0.0.0:0` and serves `/websocket` and
   `/download` on the same port with a fresh self-signed certificate. CatShare
   connects with `wss://` and `https://`, so plain HTTP does not work.
3. `WiFiP2pSender::create_group`:
   - asks NetworkManager to release `wlan0`;
   - `WpaP2pGroupOwner::new` connects to `fi.w1.wpa_supplicant1` and sets
     `NoGroupIface=true`;
   - `GroupAdd` on 5745 MHz, retry on 2412 MHz; reads SSID, PSK and frequency
     from the group object;
   - `ip addr replace 192.168.49.1/24 dev wlan0`, `ip link set wlan0 up`;
   - starts `dnsmasq` for 192.168.49.2–254, gateway/DNS 192.168.49.1.
   - if any of this fails, it falls back to a NetworkManager hotspot. That
     keeps Satella↔Satella working but Android cannot join it.
4. `BleClient::connect_and_handshake_named` finds the phone by address, then a
   5 s scan, then a unique local name; connects with retries, reads the
   receiver's `DeviceInfo`, derives the ECDH session key, encrypts `P2pInfo`
   and writes it to the P2P characteristic.
5. The phone joins the group, gets DHCP, then: server sends
   `versionNegotiation`; on ack sends `sendRequest`; phone downloads the ZIP
   from `/download?taskId=…`; status `type=1` means complete, `type=3` means
   rejected.
6. `stop_group` always runs after the group exists (also on BLE, GATT, WS and
   timeout failures): stop `dnsmasq`, `Disconnect` the group, remove
   `192.168.49.1/24`. It does **not** bring Wi-Fi back; the interface stays
   down until NetworkManager reconnects a profile.

### Files

| File | Role |
|---|---|
| `crates/satella-mta/vendor/mta-core/src/wifi/p2p_go.rs` | wpa_supplicant D-Bus GO, IP configuration, dnsmasq lifecycle, privilege escalation. |
| `crates/satella-mta/vendor/mta-core/src/wifi/p2p_sender.rs` | GO-first `create_group`, NetworkManager fallback, `ActiveHotspot` enum, `stop_group`, `DIRECT-xx` SSID. |
| `crates/satella-mta/vendor/mta-core/src/workflow/sender.rs` | TLS server, GO creation, BLE handshake, status wait, guaranteed cleanup. |
| `crates/satella-mta/vendor/mta-core/src/ble/client.rs` | Address scan, unique-name fallback, connect/read/write retries. |
| `crates/satella-mta/vendor/mta-core/src/transfer/sender_server.rs` | TLS axum server, `/websocket` protocol, ZIP `/download`. |
| `crates/satella-mta/src/lib.rs` | Satella adapter wiring, identity path, propagates the vendor task result as the send result. |
| `crates/satella-mta/vendor/mta-core/tests/p2p_go_probe.rs` | Ignored root/hardware probes for GO+DHCP, sender integration and failure cleanup. |
| `crates/satella-mta/THIRD_PARTY_NOTICES.md` | Every vendored change, for future upstream rebases. |

### Running a live send

```sh
cargo build -p satella-cli

# The phone must be on its CatShare receive screen first.
./target/debug/satella-cli discover --seconds 15

# pkexec works on this machine; sudo -n does not (password required).
timeout 180 pkexec /usr/bin/bash -c \
  'cd /home/lee/Projects/satella && RUST_LOG=info,satella_mta=info,satella_mta_vendor::wifi=debug,satella_mta_vendor::transfer=debug,satella_mta_vendor::ble=debug ./target/debug/satella-cli send <fingerprint-or-any> <file>...'

# Success markers in the log:
#   Created the P2P group DIRECT-xx on channel 5745
#   Started DHCP on wlan0
#   Writing encrypted P2P info
#   WS received: type=ack, name=versionNegotiation
#   Download request for task_id=...
#   Transfer completed successfully
#   sent N file(s)

# Cleanup / restore:
pgrep -a dnsmasq || echo "no dnsmasq"
ip -br addr show wlan0            # no 192.168.49.1/24
iw dev wlan0 info | grep type     # no P2P-GO
nmcli connection up "Mr Breast"   # local Wi-Fi profile on this machine
```

Running the whole CLI under `pkexec` means the identity is
`/root/.config/satella/mta-identity`; unprivileged runs use
`~/.config/satella/mta-identity`. The fingerprints differ between the two, so
an elevated run is a new device to peers.

### Hardware probes

```sh
cargo test -p satella-mta-vendor --test p2p_go_probe --no-run
# Run the reported binary (keep the ./ for pkexec):
timeout 120 pkexec ./target/debug/deps/p2p_go_probe-<hash> --ignored --nocapture
# Or one case:
timeout 120 pkexec ./target/debug/deps/p2p_go_probe-<hash> \
  sender_cleans_up_when_ble_fails --ignored --nocapture
```

Cases:

- `p2p_go_starts_and_serves_dhcp` — raw GO on 2412 MHz, address, dnsmasq, cleanup.
- `sender_create_group_integration` — `WiFiP2pSender::create_group` produces
  `type P2P-GO` and `192.168.49.1/24`.
- `sender_cleans_up_when_ble_fails` — ghost peer; asserts no dnsmasq, no
  address, no P2P-GO afterwards.

All three leave `wlan0` down. Restore NetworkManager when done. Rebuild the
probe after source edits: the reported hash changes, and an old binary can
hide a cleanup regression.

### Traps

- **Do not replace the GO with the NetworkManager hotspot.** NM can only make
  `mode=ap`; Android never joins it. The NM path is a Satella↔Satella fallback.
- **wpa_supplicant D-Bus is root-only.** On this machine a plain user gets
  `Access denied` from `fi.w1.wpa_supplicant1`. `pkexec` is the working
  escalation; `sudo -n` fails.
- **`NoGroupIface=true` is required** on iwlwifi; otherwise the separate group
  interface fails with `Failed to set P2P Mode: ret=-95`.
- **Android BLE addresses rotate.** The scanner's fingerprint can be stale by
  send time. The address → scan → unique-name fallback in `ble/client.rs`
  exists for this; do not remove it. Keep the unique-name check.
- **Cleanup must wrap every post-GO step**, not just the success path. The
  `transfer = async { … }.await` block plus unconditional `stop_group` in
  `workflow/sender.rs` is deliberate.
- **MTA receive uses `auto_accept=true`.** The vendor asks synchronously from
  its transfer task and the sender times out without an instant answer.
  `MtaAdapter::accept` / `reject` after the event are effectively no-ops.
- **TLS is mandatory**, and the certificate is intentionally self-signed:
  receivers accept invalid certs and validate content.
- **`dnsmasq` must be installed.** Without it the phone joins but never gets
  an address, so it hangs at “connecting”.
- **mDNS/LocalSend errors while `wlan0` is a GO are expected noise** (“Network
  is unreachable”): those sockets are bound to the old address. Functional,
  but cosmetically bad; see known gaps.
- **Rebuild the CLI after vendor edits.** `cargo check` does not update
  `target/debug/satella-cli`; a stale binary silently hides fixes.

---

## Bugs found and fixed during init

All in the adapter seam, all found because the checks exercised real protocol paths:

1. **Discovery died instantly** — `let (_discovery_stop_tx, discovery_stop_rx) = ...` dropped the vendor's stop sender on the same line; the vendor stops discovery when it drops. The adapter was announcing into a closed socket. Sender now lives in adapter state.
2. **Devices never announced themselves** — no announcement existed at all, making every Satella device invisible to LocalSend peers. `announce` is now on the trait; the runtime calls it at start.
3. **Inbound registrations dropped** — `ServerEventV2::Register` (the peer-confirmation event) was swallowed by a catch-all arm. It now feeds the discovery store.
4. **Fixed port collision** — HTTP server took `53317`, so two instances couldn't coexist. Now takes a free port and announces the port it actually bound.

---

## Known gaps

1. **Quick Share transfer leg is unverified.** Its `send`/`accept`/`reject` are fully implemented (no `todo!()`/`unimplemented!()` anywhere), and discovery is confirmed — but a real file transfer has not been exercised. This is the obvious next target.
2. **MTA sending needs root on Linux.** Android receivers only join a real Wi-Fi Direct group owner, and wpa_supplicant's D-Bus API is root-only. The CLI must run under `sudo`/`pkexec` for MTA sends; LocalSend and Quick Share stay unelevated. A privileged helper is the likely packaging fix: grant only `GroupAdd` / `Disconnect`, `ip addr`, and `dnsmasq`, keep the app unprivileged. The elevated run currently also switches the identity to `/root/.config/satella/mta-identity`.
3. **mDNS/LocalSend error spam while `wlan0` is a GO.** Sockets still point at the previous address and log “Network is unreachable”. Cosmetic; suppress or rebind while the GO is up.
4. **Windows MTA** needs a WinRT BLE implementation, not vendored. LocalSend and Quick Share paths work there.
5. **Android specifics** — BLE permissions and a foreground service for background transfers — belong to the frontend shell, not the library. No platform-specific code exists in the core, by design.
6. **One global Tokio runtime** at the UniFFI boundary (noted with a `ponytail:` comment): all adapters share one worker pool, so a busy protocol could starve another. Acceptable until Android background-transfer load proves otherwise; then per-adapter executors.

---

## Documentation

- `README.md` — scope, crate layout, UniFFI contract, CLI usage, platform notes, license origins.
- `docs/designs/satella-init.md` — the finalized design: why one trait instead of four, why the protocol lives on the peer, why GPL-3.0 is forced, the verification strategy, and the four bugs the init uncovered.
- `crates/satella-mta/THIRD_PARTY_NOTICES.md` — fork provenance plus every change made to the vendored MTA engine, for rebases.
- `crates/satella-mta/vendor/mta-core/src/wifi/p2p_go.rs` — module docs explain why D-Bus, why root, and why `NoGroupIface`.
- `crates/satella-mta/vendor/mta-core/tests/p2p_go_probe.rs` — executable form of the runbook above.