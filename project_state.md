# Satella — State of the Project

**Stage:** v0.1.0, greenfield init complete and verified, committed.
**Last live verification:** 2026-11-24 — unprivileged sudo-less helper path re-verified end to end (no outer `pkexec`, no password prompt, stable user identity); 2026-11-23 — MTA send and receive against a OnePlus 13.

---

## Layout

| Crate | Role | Lines (src) |
|---|---|---|
| `satella-core` | Protocol-neutral types + the `ProtocolAdapter` trait. No I/O, no protocol logic. | 399 |
| `satella-runtime` | Runs adapters concurrently, merges events, routes sends by protocol. | 182 |
| `satella-localsend` | LocalSend v2 adapter (vendored engine, 560K). | 922 |
| `satella-quickshare` | Quick Share adapter (vendored engine, 336K). | 370 |
| `satella-mta` | MTA adapter (vendored engine, 244K). | 395 |
| `satella-mta-helper` | Narrow Linux helper: fixed `create`/`destroy` for the P2P GO. | 282 |
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
- Unprivileged helper path (2026-11-24): built and installed `satella-mta-helper` plus the polkit action/rule; ran `sender_create_group_integration` and `sender_cleans_up_when_ble_fails` as user `lee` with `SATELLA_MTA_HELPER=/usr/libexec/satella-mta-helper`, no outer `pkexec` and no password prompt. Both passed; no dnsmasq, GO address, P2P-GO or pid file survived. A full CLI send against the phone was not re-run in this session: discovery saw 0 BLE devices (phone not advertising), so the phone-side leg still rests on the 2026-11-23 evidence.
- In-process backend (2026-11-24): `sender_create_group_integration` under `pkexec` also passed, so the root/probe path survived the backend refactor and cleaned up the same way.
- Re-verification of the sudo-less path (2026-11-24, independent of the implementing session): as uid 1000, with no outer `pkexec`, `sender_create_group_integration` and `sender_cleans_up_when_ble_fails` both passed three times in a row against the installed `/usr/libexec/satella-mta-helper` — no password prompt on any invocation, confirming the permanent `Result.YES` grant. The action resolves to `org.satella.mta.manage-p2p` with `org.freedesktop.policykit.exec.path -> /usr/libexec/satella-mta-helper`, so the grant covers only that binary. The helper refuses to run as non-root, rejects unknown commands, unsafe interface names and out-of-band frequencies (all exit 1), and `destroy` kills only the PID in the root-owned pid file after confirming `/proc/<pid>/comm` is `dnsmasq`. After each run no GO address, dnsmasq, P2P-GO interface or pid file survived. `satella-cli discover` ran unprivileged and reused the existing user-owned `~/.config/satella/mta-identity` (mtime unchanged), so the BLE fingerprint stays stable. The phone-side leg still rests on the 2026-11-23 evidence: discovery saw 0 BLE devices during this session.
- Wi-Fi restore after P2P cleanup (2026-11-24): the sender now re-activates the Wi-Fi profile that was active before the P2P group was created. Verified through the real sender path as uid 1000 via the helper: connected to `Mr Breast` → P2P group → cleanup → `GENERAL.STATE: 100 (connected)`, `GENERAL.REASON: 0`, same profile re-activated. The "already disconnected" case was verified to stay disconnected, so the sender never hijacks a network the user did not choose. The new probe `sender_restores_wifi_after_the_group_is_torn_down` was falsified: with the restore disabled it fails (`AFTER profile=None`), and it passes with the fix in place.

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
3. `WiFiP2pSender::create_group` asks NetworkManager to release `wlan0`, then
   picks a `GroupOwnerBackend`:
   - as root, `InProcessGroupOwner` talks to `fi.w1.wpa_supplicant1` directly,
     sets `NoGroupIface=true`, calls `GroupAdd`, configures the address and
     starts `dnsmasq`;
   - as an ordinary user, `HelperGroupOwner` calls `satella-mta-helper` once
     through `pkexec`. The helper does the same root-only steps and prints the
     group as JSON. The helper exits; the P2P group and `dnsmasq` keep running.
   Both try 5745 MHz, then 2412 MHz. If all of this fails, the sender falls
   back to a NetworkManager hotspot. That keeps Satella↔Satella working but
   Android cannot join it.
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
   `192.168.49.1/24`, and then re-activate the Wi-Fi profile that was active
   before the group was created. The restore has to happen after the group is
   gone, otherwise NetworkManager fights the group and tears it down again.
   It is skipped when there was no active connection to begin with.

### Files

| File | Role |
|---|---|
| `crates/satella-mta/vendor/mta-core/src/wifi/p2p_go.rs` | wpa_supplicant D-Bus GO, IP configuration, dnsmasq lifecycle, `GroupOwnerBackend`, in-process and helper backends. |
| `crates/satella-mta/vendor/mta-core/src/wifi/p2p_sender.rs` | GO-first `create_group`, backend selection, NetworkManager fallback, `ActiveHotspot` enum, `stop_group`, Wi-Fi restore, `DIRECT-xx` SSID. |
| `crates/satella-mta-helper/src/main.rs` | Narrow root helper: fixed `create`/`destroy`, interface/frequency validation, JSON output, one operation per process. |
| `packaging/org.satella.mta.policy` | Dedicated polkit action bound to `/usr/libexec/satella-mta-helper`. |
| `packaging/60-satella-mta.rules` | Permanent `polkit.Result.YES` for local `wheel`, so later sends never prompt. |
| `crates/satella-mta/vendor/mta-core/src/workflow/sender.rs` | TLS server, GO creation, BLE handshake, status wait, guaranteed cleanup. |
| `crates/satella-mta/vendor/mta-core/src/ble/client.rs` | Address scan, unique-name fallback, connect/read/write retries. |
| `crates/satella-mta/vendor/mta-core/src/transfer/sender_server.rs` | TLS axum server, `/websocket` protocol, ZIP `/download`. |
| `crates/satella-mta/src/lib.rs` | Satella adapter wiring, identity path, propagates the vendor task result as the send result. |
| `crates/satella-mta/vendor/mta-core/tests/p2p_go_probe.rs` | Ignored root/hardware probes for GO+DHCP, sender integration, failure cleanup and Wi-Fi restore. |
| `crates/satella-mta/THIRD_PARTY_NOTICES.md` | Every vendored change, for future upstream rebases. |

### Running a live send

```sh
cargo build -p satella-cli -p satella-mta-helper

# One-time install (needs root once). The installed helper path is what the
# polkit action authorizes; after this the rule grants it permanently.
id -nG   # the rule requires the local user to be in wheel
sudo install -m755 target/debug/satella-mta-helper /usr/libexec/satella-mta-helper
sudo install -m644 packaging/org.satella.mta.policy /usr/share/polkit-1/actions/
sudo install -m644 packaging/60-satella-mta.rules /etc/polkit-1/rules.d/

# The phone must be on its CatShare receive screen first.
./target/debug/satella-cli discover --seconds 15

# No outer pkexec: the CLI stays a user process and invokes the helper itself.
# The debug helper next to the CLI takes precedence over /usr/libexec, so pin
# the installed one to get the permanent (no-prompt) polkit grant:
timeout 180 env \
  SATELLA_MTA_HELPER=/usr/libexec/satella-mta-helper \
  RUST_LOG=info,satella_mta=info,satella_mta_vendor::wifi=debug,satella_mta_vendor::transfer=debug,satella_mta_vendor::ble=debug \
  ./target/debug/satella-cli send <fingerprint-or-any> <file>...

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
nmcli -f GENERAL.STATE dev show wlan0   # connected again (see below)
```

The sender restores the Wi-Fi connection that was active before the P2P group
was created, so `wlan0` comes back on its own profile. NetworkManager's
`Device.disconnect()` is a *user-requested* disconnect: it sets the device's
autoconnect block, so NM never reconnects by itself and the interface used to
stay down after every send. If the interface had no connection to begin with,
nothing is restored.

Normal runs keep the identity at `~/.config/satella/mta-identity`; the old
`/root/.config/satella/mta-identity` from whole-CLI elevations is obsolete.
The helper path is chosen as `SATELLA_MTA_HELPER`, then next to the running
binary (including `target/debug` for tests in `deps`), then
`/usr/libexec/satella-mta-helper`. Without the polkit rule, `pkexec` asks for
authorization once per helper invocation (`create` and `destroy`); with the
rule installed it never asks.

### Hardware probes

```sh
cargo test -p satella-mta-vendor --test p2p_go_probe --no-run
PROBE=./target/debug/deps/p2p_go_probe-<hash>

# Sender cases as an ordinary user exercise HelperGroupOwner (helper installed):
timeout 120 $PROBE sender_create_group_integration --ignored --nocapture
timeout 120 $PROBE sender_cleans_up_when_ble_fails --ignored --nocapture
timeout 120 $PROBE sender_restores_wifi_after_the_group_is_torn_down --ignored --nocapture

# The raw D-Bus case has no helper wrapper, so it still needs root:
timeout 120 pkexec $PROBE p2p_go_starts_and_serves_dhcp --ignored --nocapture
```

Running a sender case under `pkexec` instead exercises the in-process backend.

Cases:

- `p2p_go_starts_and_serves_dhcp` — raw GO on 2412 MHz, address, dnsmasq, cleanup.
- `sender_create_group_integration` — `WiFiP2pSender::create_group` produces
  `type P2P-GO` and `192.168.49.1/24`.
- `sender_cleans_up_when_ble_fails` — ghost peer; asserts no dnsmasq, no
  address, no P2P-GO afterwards.
- `sender_restores_wifi_after_the_group_is_torn_down` — asserts the Wi-Fi
  profile that was active before the send is active again afterwards. Needs a
  Wi-Fi connection up beforehand; it panics otherwise.

The sender cases restore the Wi-Fi connection themselves. The raw D-Bus case
still leaves `wlan0` down, so restore NetworkManager after it by hand.
Rebuild the probe after source edits: the reported hash changes, and an old
binary can hide a cleanup regression.

### Traps

- **Do not replace the GO with the NetworkManager hotspot.** NM can only make
  `mode=ap`; Android never joins it. The NM path is a Satella↔Satella fallback.
- **wpa_supplicant D-Bus is root-only.** On this machine a plain user gets
  `Access denied` from `fi.w1.wpa_supplicant1`. That is why ordinary-user
  sends go through `satella-mta-helper`; `pkexec` is the working escalation and
  `sudo -n` fails. Do not open the whole wpa_supplicant D-Bus API to users.
- **The polkit action is bound to `/usr/libexec/satella-mta-helper`.** A dev
  build in `target/debug` works through generic `pkexec` but prompts each time;
  only the installed path gets the permanent `Result.YES` rule.
- **The helper's pid file is the kill authority.** `destroy --dhcp-pid` is only
  a consistency check. Never kill a caller-supplied pid; read
  `/run/satella-mta/dnsmasq-<iface>.pid` and verify `/proc/<pid>/comm` first.
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
2. **MTA sending needs a one-time Linux helper install.** Android receivers only join a real Wi-Fi Direct group owner, and wpa_supplicant's D-Bus API is root-only. The privileged operations now live in `satella-mta-helper` plus a packaged polkit action, so the CLI stays unprivileged and keeps `~/.config/satella/mta-identity`. Without that install, MTA send still needs the whole CLI under `sudo`/`pkexec`, or it falls back to the Satella-only NetworkManager hotspot. Distro packaging for the helper/policy files is still manual.
3. **mDNS/LocalSend error spam while `wlan0` is a GO.** Sockets still point at the previous address and log “Network is unreachable”. Cosmetic; suppress or rebind while the GO is up.
4. **NetworkManager keeps a stale `p2p-dev-wlan0` device.** The kernel interface is gone after the group is destroyed, but NM still lists the P2P device object. It does not block the Wi-Fi restore, which is re-activated explicitly. Cosmetic.
5. **Windows MTA** needs a WinRT BLE implementation, not vendored. LocalSend and Quick Share paths work there.
6. **Android specifics** — BLE permissions and a foreground service for background transfers — belong to the frontend shell, not the library. No platform-specific code exists in the core, by design.
7. **One global Tokio runtime** at the UniFFI boundary (noted with a `ponytail:` comment): all adapters share one worker pool, so a busy protocol could starve another. Acceptable until Android background-transfer load proves otherwise; then per-adapter executors.

---

## Documentation

- `README.md` — scope, crate layout, UniFFI contract, CLI usage, platform notes, license origins.
- `docs/designs/satella-init.md` — the finalized design: why one trait instead of four, why the protocol lives on the peer, why GPL-3.0 is forced, the verification strategy, and the four bugs the init uncovered.
- `crates/satella-mta/THIRD_PARTY_NOTICES.md` — fork provenance plus every change made to the vendored MTA engine, for rebases.
- `crates/satella-mta/vendor/mta-core/src/wifi/p2p_go.rs` — module docs explain why D-Bus, why root, and why `NoGroupIface`; the backend trait isolates the privileged transport.
- `crates/satella-mta-helper/` — the fixed-purpose root helper and its JSON protocol.
- `packaging/` — the polkit action and permanent rule installed by the runbook.
- `crates/satella-mta/vendor/mta-core/tests/p2p_go_probe.rs` — executable form of the runbook above.