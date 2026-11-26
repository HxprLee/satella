# Satella — State of the Project

**Stage:** v0.1.0, greenfield init complete and verified, committed. Daemon mode and its desktop notifications added to `satella-cli` (2026-11-25).
**Last live verification:** 2026-11-25 — daemon mode: detached start, `status`, `stop`, stale and corrupt pid files, alias; the notification service against a live D-Bus session (a notification was shown and the bus answered); 2026-11-24 — unprivileged sudo-less helper path re-verified end to end (no outer `pkexec`, no password prompt, stable user identity); 2026-11-23 — MTA send and receive against a OnePlus 13.

---

## Layout

| Crate | Role | Lines (src) |
|---|---|---|
| `satella-core` | Protocol-neutral types + the `ProtocolAdapter` trait. No I/O, no protocol logic. | 410 |
| `satella-runtime` | Runs adapters concurrently, merges events, routes sends by protocol. | 182 |
| `satella-localsend` | LocalSend v2 adapter (vendored engine, 560K). | 932 |
| `satella-quickshare` | Quick Share adapter (vendored engine, 336K). | 478 |
| `satella-mta` | MTA adapter (vendored engine, 244K). | 467 |
| `satella-mta-helper` | Narrow Linux helper: fixed `create`/`destroy` for the P2P GO. | 282 |
| `satella-api` | The public UniFFI surface (`satella.udl`). | 483 |
| `satella-cli` | Terminal harness: discover / send / receive, plus a daemon mode with desktop notifications. | 1972 |

The three vendored crates are excluded from the workspace so upstream's own warnings don't pollute Satella's gates.

---

## Daemon mode

`satella-cli daemon` runs the engine as a detached background process, so the
device keeps receiving files while nobody is watching the terminal. `status`
reports what it sees and `stop` shuts it down.

| File | Role |
|---|---|
| `crates/satella-cli/src/daemon.rs` | The daemon itself: detach, the event loop, the pid/state files, `status` and `stop`. |
| `crates/satella-cli/src/peers.rs` | The merged peer table, shared by `discover` and the daemon. |
| `crates/satella-cli/src/transfer.rs` | The handling of an incoming transfer, shared by `receive` and the daemon. |
| `crates/satella-cli/src/notify.rs` | The desktop notification service: the D-Bus proxy, the two buttons, and the clicks. |
| `crates/satella-cli/src/logging.rs` | The log filter, and the file-backed subscriber the daemon uses. |
| `crates/satella-cli/tests/daemon.rs` | The checks of the peer table and the state file. |
| `crates/satella-cli/tests/notifications.rs` | The checks of the notification bookkeeping and the flag. |
| `crates/satella-cli/tests/notification_bus.rs` | The check of the notification service against a bus of its own. |

**It detaches by re-executing itself, not by `fork`.** The CLI is
`#[tokio::main]`, so the Tokio runtime's worker threads already exist when
`main()` runs. A forked child would inherit a broken runtime: only the forking
thread survives and the reactor is gone. The parent therefore sets `setsid` in
the `pre_exec` hook of a `Command` (which runs between `fork` and `exec`,
where there is one thread) and execs the binary again with a hidden
`--daemon-child` flag. The child re-enters `main()`, writes its pid file,
reports its readiness on a socket, and runs the event loop.

**The readiness handshake is what makes "started" true.** The parent waits for
the child's `READY <pid>` line before it prints anything. A daemon that dies
right after it starts is therefore reported as a failure, not as a success.

**The pid file is checked, not trusted.** A pid file that is left over from a
crashed daemon names a process that is gone. `status` and `stop` call
`kill(pid, 0)`, which distinguishes a live process from `ESRCH` (gone) and
`EPERM` (somebody else's). The file also holds the process's start time from
`/proc/<pid>/stat`, which is what tells a recycled pid from the daemon that
owns the file: `stop` refuses to signal a pid that was reused.

**The state file is written atomically.** It is written next to its target and
renamed over it, so `status` never reads half a file. It is only rewritten
when the peer set or the transfer list actually changed, because discovery
re-announces peers every few seconds.

### Files the daemon owns

| Path | What it holds |
|---|---|
| `~/.cache/satella-cli/satella-cli.pid` | The pid, the start time, the process start ticks, the download directory. |
| `~/.cache/satella-cli/satella-cli.state.json` | The peers of every protocol and the transfers that were handled. |
| `~/.cache/satella-cli/satella-cli.log` | The log of the daemon. `RUST_LOG` applies; the default is `info` for the daemon and `warn` for the foreground commands. |

### The desktop notification

The daemon asks the user through a desktop notification before it takes a
transfer. The notification carries an **Accept** and a **Refuse** button, and
the click decides what happens to the transfer.

**The notification is the FreeDesktop specification, spoken over the session
bus.** `crates/satella-cli/src/notify.rs` writes the D-Bus proxy out by hand
rather than pulling `notify-rust` in: one call and one signal are not worth a
second D-Bus stack. The `zbus` version is the one the vendored MTA engine
already uses, so `Cargo.lock` gained no new crate names at all.

**The daemon's loop does not block on the question.** The clicks arrive on
their own branch of the `tokio::select!`, next to the runtime's event stream
and the shutdown signal, so a transfer that waits for the user does not stop
the peers from being discovered or a second transfer from arriving. The
waiting transfers live in the `Awaiting` map, keyed by the id of the
notification that asks about them, so a click reaches the transfer it was
about and two transfers can wait at once.

**Nothing times the question out.** The sender waits for the answer and
gives up on its own terms; the adapters report that through
`AdapterEvent::ReceiveExpired`, and the daemon takes the notification down
with it. There is no client-side timer, so a question the user ignores dies
with the transfer rather than sitting on the screen forever.

**A machine without a session bus cannot ask.** The daemon takes the
transfers itself then, and says so in the log. The same happens if the bus
dies under a running daemon: taking the transfer is better than leaving the
sender waiting.

**The MTA engine had to change to make this real.** Its receive callback was
synchronous, so it could only ever answer "yes" instantly — a refusal could
not exist. The vendored `ReceiverCallback::on_send_request` and
`ReceiveProgressCallback::on_request` are now `async`, and the question
travels with a `oneshot::Sender<bool>` that the application answers later.
The adapter holds the senders by transfer id, so `accept` and `reject` reach
the right transfer when two of them arrive at once.

### Traps

- **The question has no timer of its own.** The daemon does not time the
  user out; the sender waits for the answer and gives up on its own terms.
  A question nobody answers dies with the transfer it was about, through the
  `AdapterEvent::ReceiveExpired` event the adapters emit.
- **The daemon is not a service.** It has no `systemd` unit and no control
  socket: `status` reads a state file, it does not talk to the running
  process. There is no way to send a file from another process.
- **The daemon runs unprivileged.** MTA sends go through `satella-mta-helper`
  as usual, so the daemon needs the helper installed and `pkexec` to work.
  The `SATELLA_MTA_HELPER` variable has to be in the daemon's environment.
- **Do not add `fork`.** The same trap as in the MTA helper: `fork` after the
  Tokio runtime has started is not safe. Re-exec is the way to detach.

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
- `satella-cli/tests/daemon.rs` (7 tests) — the peer table dedupes by protocol and fingerprint; a lost peer leaves the table; an update is a change only when it differs; the state file and the pid file round-trip through JSON; a stale pid is not a running daemon.
- `satella-cli/tests/notifications.rs` (8 tests) — a click reaches its own transfer; an unknown click decides nothing; two transfers wait at once and each click takes its own; an expired transfer takes its question with it; the daemon asks by default and `--auto-accept` turns the asking off.
- `satella-cli/tests/notification_bus.rs` (1 test) — against a bus of its own with a fake notification service: the notification really is shown, it really carries the two buttons, and the click really comes back as the right decision.
- `satella-cli/src/notify.rs` (4 unit tests) — the notification body names the files and the total; a size is shown in a readable unit.
- `satella-localsend/tests/discovery.rs` (1 test) — two *real* LocalSend adapters finding each other via actual multicast + HTTP register. Not a stub.
- `satella-localsend/tests/identity.rs` (5 tests) — a certificate survives a restart, a corrupt file is replaced, two instances get distinct identities.
- `satella-api/tests/identity.rs` (1 test) — `Engine::with_storage_dir` writes the certificate where the platform asked.
- Vendored upstream suites also run: **99 + 35 + 13 + 5** tests, all green.
- `crates/satella-quickshare/vendor/rqs-core/src/hdl/session_tests.rs` (4 tests) — a disconnection frame ends both handlers without an error; a finished transfer is not downgraded to a disconnection; the session states that mean "the session is over" are the ones the manager does not report as failures.
- `crates/satella-quickshare/vendor/rqs-core/src/utils.rs` (5 tests) — the default route is the row with the zero destination, the address to advertise is the one in the gateway's subnet, an address on another subnet of the same interface is never chosen, and the choice holds against the host's real route table.
- `crates/satella-quickshare/vendor/rqs-core/src/hdl/mdns.rs` (1 test) — the advertisement names exactly one address and does not have `addr_auto` on.
- `crates/satella-mta/vendor/mta-core/tests/p2p_go_probe.rs` — 3 **ignored** root/hardware probes. They are manual validation, not CI; see the runbook below.

**Live evidence on this machine:**
- Daemon mode (2026-11-25): `satella-cli daemon` started, detached (own session id, no controlling terminal, reparented to `systemd --user`), and survived the shell that started it. `status` reported the pid and the state file was valid JSON; `stop` shut it down and removed the pid file. A second `daemon` refused to start while one was running. A pid file naming a dead process was reported as "not running" and cleaned up by `stop`, and a corrupt pid file was reported as corrupt and replaced on the next start. `--alias` reached the BLE advertisement (`device_name='Test Daemon'`). All three adapters came up in the daemon, exactly as in `discover`.
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
- **MTA receive waits for the answer now.** The vendor's receive callback is
  `async` and its question travels with a `oneshot::Sender<bool>`, so the
  sender waits for the application's decision instead of for an instant yes.
  `MtaAdapter::accept` / `reject` answer that question; they are not no-ops.
  The `auto_accept` flag on the vendor's side is what made that possible.
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

## BLE pairing (why a phone used to ask to be paired)

**Symptom:** sending or receiving over MTA made the phone pop up a Bluetooth
pairing request. MTA does not need pairing — SSID/PSK/MAC are protected by an
application-level ECDH + AES-256-CTR exchange
(`vendor/mta-core/src/crypto/ble_security.rs`), over a plain, unbonded BLE link.

**Root cause (measured, 2026-11-25): the MTA adapter turned the shared
adapter unpairable.** The MTA adapter always starts a GATT *server* in the
background (`satella-mta/src/lib.rs`, the `Receiver` task) — including while
sending. That server called `adapter.set_pairable(false)`, and because the
adapter is shared, the *send* path's GATT-client connect ran against a
non-pairable adapter. The phone (CatShare) starts SMP when it brings up its
GATT server; the kernel refused the bonding outright, so BlueZ never got
`ServicesResolved`, and `bluez-async` gave up after its hardcoded 5 s
(`SERVICE_DISCOVERY_TIMEOUT`) with `Service discovery timed out`.

Evidence from the failing run and from direct probing of the phone:

| Probe | Result |
|---|---|
| Phone's MTA characteristics | `00009954` / `00009953`, flags `read`/`write` only — **no** `encrypt`/`authenticate` |
| Phone's `GattCharacteristic1.MTU` | **517** (payload limit 514) — the ~266-byte encrypted P2P JSON fits in one packet |
| `bluetoothctl connect` (no MTA running) | connects, stays `Paired: no`, services resolve |
| `btleplug`/`bluez-async` connect | fails after **5.97 s** with `ServiceDiscoveryTimedOut` — bluez-async's 5 s constant |

The MTU and the "no encrypt flags" facts rule out the long-write /
`INSUFFICIENT_ENCRYPTION` escalation as the trigger here: the phone never asks
for encryption on these characteristics, and the payload fits in a single
packet anyway. The escalation theory still explains the *receive* direction,
where the phone shows its pairing sheet, but it is not what broke the send.

BlueZ escalates the ATT security level on its own when an ATT request comes
back with `INSUFFICIENT_ENCRYPTION` (0x0F) or `AUTHENTICATION` (0x05): it raises
the channel to `BT_ATT_SECURITY_MEDIUM` and retries (BlueZ
`src/shared/att.c`, `change_security()`). MEDIUM-level links can only be
obtained through SMP pairing, and the pairing request goes to BlueZ's
**default** agent (BlueZ `src/agent.c`, `agent_get()`), which on this desktop
is `blueman`, which shows the PIN dialog. That is why the phone pops a sheet
even though MTA itself never asks for encryption.

### Send and receive are not symmetric

| Direction | Host role | Who owns the GATT chars | Pairing prompts | Transfer |
|---|---|---|---|---|
| Satella **receive** (phone sends) | GATT **server** | Satella — plaintext read/write, no encrypt flags | Host: no. Phone: yes | Succeeds |
| Satella **send** (phone receives) | GATT **client** | Phone CatShare | Host: no. Phone: yes | Failed |

Both directions let the phone start SMP; the difference is that the host
is the GATT *server* on receive (so ATT completes even though the phone shows
a pairing sheet), while on send the host is the GATT *client* and BlueZ blocks
the connect on the unresolved pairing.

### Rejected pairing is not the same as no pairing

An earlier revision registered an always-rejecting BlueZ **default** agent in
both `BleClient::new()` and `GattServer`. That silenced the host dialog but did
not stop SMP. On the send path it was actively harmful: BlueZ-as-client waits
for `ServicesResolved`, and a rejected pairing means it never arrives, so the
send failed with `Service discovery timed out` / `Not connected` while the phone
sat on its pairing sheet. A default agent also *invites* the negotiation —
BlueZ only asks an agent because an agent is registered.

**Current behaviour (all in `vendor/mta-core/src/ble/`):**

| Change | File | Why |
|---|---|---|
| **Never set `Pairable=false` on the adapter** | `server.rs` | The adapter is shared between send and receive. Turning it unpairable makes the kernel refuse the phone's SMP, so `ServicesResolved` never arrives and every send dies with `Service discovery timed out`. "No pairing dialog" is achieved by *not registering an agent*, not by closing the adapter. |
| No pairing agent on the send path | `client.rs` | A rejected pairing makes BlueZ-as-client stall on `ServicesResolved`. The fix is to avoid triggering SMP, not to refuse it. |
| No pairing agent on the receive path either | `server.rs`, `security.rs` | Registering an agent makes BlueZ run the SMP exchange; with no agent registered BlueZ rejects the bonding outright and the desktop stays quiet. |
| Don't retry security errors | `client.rs` | `read_with_retry`/`write_with_retry` used to retry 3×; each retry re-drove the pairing escalation. Security errors now fail immediately as `BleClientError::PairingRequired`. |
| Read the negotiated ATT MTU and require a single-packet write | `client.rs`, `security.rs` | Kept as a guard, not as the fix: the encrypted P2P JSON is ~266 bytes and the phone reports MTU 517, so it fits. If it ever does not fit, failing fast with `BleClientError::AttMtuTooSmall` is right — long-writing is where phones answer `INSUFFICIENT_ENCRYPTION`, which is what starts SMP. |
| Map connect-time pairing stalls to `PairingRequired` | `client.rs` | `Service discovery timed out` during `connect()` means SMP was in flight; it now surfaces as `PairingRequired` rather than an opaque IO error, and is not retried. |
| Restore the discovery filter after a scan | `scanner.rs` | A wide-open filter left behind affects other scanners. |

**Note on the MTU:** `btleplug` does not expose the ATT MTU, so the client opens
a parallel `bluer` session and reads `org.bluez.GattCharacteristic1.MTU` on the
P2P characteristic. bluer subtracts 5 for the socket workaround, so the value
read is slightly conservative — good enough to decide single-packet vs
long-write.

**Tests:** `cargo test -p satella-mta-vendor --lib` covers the MTU arithmetic
(`ble::security::tests`) and the security-error classification
(`ble::client::tests`), which pins the exact D-Bus spellings
(`"Not paired"`, `"NotPermitted"`, `"AuthenticationFailed"`) — note that
`dbus::Error`'s `Display` prints only the message, so the error *name* appears
solely in `Debug`.

---

## Quick Share session teardown

**Symptom:** sending a file to a Quick Share peer ended with two errors on an
otherwise successful transfer:

```
ERROR satella_quickshare_vendor::hdl::outbound: Unhandled offline frame encrypted:
  OfflineFrame { version: Some(V1), v1: Some(V1Frame { r#type: Some(Disconnection), ...
ERROR satella_quickshare_vendor::manager: TcpServer: error while handling client:
  early eof (Finished)
```

**Root cause:** a peer closes the session with a `DISCONNECTION` frame and
then hangs up. The encrypted frame handling only knew `PAYLOAD_TRANSFER` and
`KEEP_ALIVE`, so the disconnection fell through to a catch-all that logged it
as an unhandled frame and carried on. The socket then ended, and the manager
reported that end as a failure — even though the session was already over.

**Two defects, one symptom:**

| File | Change | Why |
|---|---|---|
| `vendor/rqs-core/src/hdl/outbound.rs` | The encrypted frame handling answers `FrameType::Disconnection`: it sets the state to `Disconnected` and returns `AppError::NotAnError`. | The frame is the peer saying the session is over. It is not an error, and the engine has to stop reading the socket. |
| `vendor/rqs-core/src/hdl/inbound.rs` | The same answer on the receiving side. | The same frame arrives there, and the transfer that was waiting for a decision dies with the session. |
| `vendor/rqs-core/src/manager.rs` | A socket that ends after the session was over is logged at debug level, through the new `is_terminal` helper. | The end of the socket is the normal end of a transfer. Reporting it as an error buried the failures that matter. |

**A finished transfer stays finished.** The peer sends the disconnection right
after the last byte, so the frame arrives after the transfer is over. The
handler checks the state first and does not downgrade a `Finished` transfer to
a `Disconnected` one.

**Checks:** `crates/satella-quickshare/vendor/rqs-core/src/hdl/session_tests.rs`
(4 tests) drives the real handlers over a real socket pair, with the real
encryption, so the frame the peer sends is the frame the engine has to
understand. The checks were falsified: reverting the outbound fix fails the
two outbound checks, reverting the inbound fix fails the inbound one.

---

## Quick Share advertisement (one address, or the peer gives up)

**Symptom:** a phone could *discover* Satella over Quick Share but could not
send a file to it. The device was listed, the send never started, no consent
prompt appeared, no file arrived.

**Root cause:** the mDNS advertisement named **two** IPv4 addresses for one
service. `vendor/rqs-core/src/hdl/mdns.rs` asked for automatic addressing
(`enable_addr_auto(AddrType::V4)`), and the mDNS fork sweeps in *every*
non-loopback address of the host. An interrupted MTA send had left its
Wi-Fi Direct group owner address `192.168.49.1/24` on `wlan0`, so the service
was answered twice — once from `192.168.1.5` and once from `192.168.49.1` —
with the same SRV target and a different A record each time. The peer had two
candidates for one service, one of them unroutable from the phone's subnet,
and the transfer never began.

**The trap:** the mDNS daemon opens one socket per host address and answers on
every one of them. An address a peer cannot route to is not ignored — it is a
second, competing answer for the same service. Any secondary address on the
machine (a Wi-Fi Direct group owner, a VPN, a Docker bridge) does this.

**The fix:** the advertisement names exactly one address — the one of the
default route's subnet. `vendor/rqs-core/src/utils.rs` reads the default route
from `/proc/net/route` and picks the address of that interface that is in the
gateway's subnet; `mdns.rs` passes that address to `ServiceInfo::new` and
leaves `enable_addr_auto` off. The flag has to stay off: the fork re-adds a
newly appearing address to an `addr_auto` service within 30 seconds, so
dropping it is what makes the single address stick. The service is rebuilt and
re-registered when the address to advertise changes, so a host that comes up
without a network or moves to another one still advertises the right address.

**Checks:** `vendor/rqs-core/src/utils.rs` (5 tests) and
`vendor/rqs-core/src/hdl/mdns.rs` (1 test) — the selected address is in the
gateway's subnet, an address on another subnet of the same interface is never
chosen, and the built `ServiceInfo` holds exactly one address with
`addr_auto` off.

**Verified on the wire:** with `192.168.49.1/24` still on `wlan0`, the
service is answered from `192.168.1.5` only, with a single `A 192.168.1.5`
record. Before the fix the same host answered from both addresses.

**Also fixed:** the accept of an inbound connection was logged at `trace`,
below the daemon's `info` filter, so a connection that was accepted and then
dropped left no record at all. It is now `info`.

---

## Quick Share send reported a refused transfer as a success

**Symptom:** sending a file to a Quick Share peer always printed

```
sending to 一加 13 over Quick Share
sent 1 file(s)
```

even when the peer refused the transfer or never answered the request. The
two lines appeared within the same second, so no session had happened at all.

**Root cause:** the send answered before the peer had said anything.

| Layer | What it did | What was missing |
|---|---|---|
| `satella-cli` | `runtime.send(request).await` returned `Ok(())`, and the command printed `sent N file(s)`. | No wait for the peer's consent, and no failure path at all. |
| `satella-quickshare` | `QuickShareAdapter::send()` pushed a `SendInfo` into the engine's channel and returned `Ok(())`. | The channel only carries the *request*. The outcome of the transfer had no way back to the caller. |
| `vendor/rqs-core` | The real session ran in `TcpServer::connect()`. It knew the outcome — the vendor even logs `Cannot process: consent denied` — but wrote it into a broadcast nobody was reading for outbound transfers. | No channel from the session back to the caller that started it. |

Quick Share was the only adapter with this shape: MTA waits on the vendor's
`SendEvent::Complete`/`SendEvent::Error` and LocalSend waits on the HTTP
`prepare-upload` response, which is a `403 Rejected` when the peer says no.

**Two defects behind the one symptom:**

1. **The outcome had no way back.** The engine now answers every send over a
   registry of one-shot receipts, keyed by the id of the transfer
   (`vendor/rqs-core/src/outcome.rs`). The receipt says what happened:
   `Completed`, `Rejected`, `TimedOut`, or `Failed(reason)`. The registry is
   used instead of a field inside `SendInfo` because `SendInfo` is
   serializable and its TypeScript binding is generated from it, and a
   one-shot sender is neither.
2. **A peer that never answers hung forever.** The engine read the socket
   without a deadline, so a peer that ignored the request kept the session
   pending forever and no receipt was ever produced. The wait is now bounded
   (30s by default, `Config::consent_timeout` overrides it). It covers the
   handshake and the consent only: once the files are on their way a slow
   peer is not a lost one.

**A refusal is not a hang-up.** The refusal path recorded
`State::Disconnected`, which is what a peer that hung up records, so even a
caller that listened could not tell "the peer said no" from "the peer
vanished". The refusal now records `State::Rejected`.

**The adapter waits, and it does not hold the lock.** `send()` takes the
`send_tx` and the registry, releases the lock, and only then waits for the
receipt — holding it for the whole session would stop the adapter from being
stopped. The transfer id carries a UUID, because two transfers to the same
peer would otherwise share one receipt.

**Checks:**
- `vendor/rqs-core/src/hdl/outcome_tests.rs` (4 tests): a refusal is a
  rejection and not a disconnection, a peer that never answers ends the
  session, the receipt names the outcome, a transfer nobody waits for is not
  an error.
- `crates/satella-quickshare/tests/outcome.rs` (2 tests): a send to a peer
  that never answers fails, a send to a peer that hangs up fails.
- `crates/satella-cli/tests/send_report.rs` (1 test): a refused transfer is
  not reported as a sent file.

The checks were falsified: reverting the `State::Rejected` change fails the
refusal check, and making the adapter answer before the receipt fails both
adapter checks.

---

## Bugs found and fixed during init

All in the adapter seam, all found because the checks exercised real protocol paths:

1. **Discovery died instantly** — `let (_discovery_stop_tx, discovery_stop_rx) = ...` dropped the vendor's stop sender on the same line; the vendor stops discovery when it drops. The adapter was announcing into a closed socket. Sender now lives in adapter state.
2. **Devices never announced themselves** — no announcement existed at all, making every Satella device invisible to LocalSend peers. `announce` is now on the trait; the runtime calls it at start.
3. **Inbound registrations dropped** — `ServerEventV2::Register` (the peer-confirmation event) was swallowed by a catch-all arm. It now feeds the discovery store.
4. **Fixed port collision** — HTTP server took `53317`, so two instances couldn't coexist. Now takes a free port and announces the port it actually bound.

---

## Known gaps

1. **Quick Share transfer leg is only half verified.** Its `send`/`accept`/`reject` are fully implemented (no `todo!()`/`unimplemented!()` anywhere), and discovery is confirmed. A real file transfer **receive** from a OnePlus 13 was exercised end to end (2026-11-26: consent, transfer, file written). The **send** direction reaches the peer and the peer answers, but the session teardown was reported as a failure (see "Quick Share session teardown" below); that is now fixed and the checks cover it, but a real send against a phone has not been re-run since. That is the obvious next target. The send also used to report a refused transfer as a success (see "Quick Share send reported a refused transfer as a success" above); that is fixed and the checks cover it, but it has not been re-run against the phone either.
2. **The inbound Quick Share failure was fixed but not re-tested against the phone.** The advertisement named two addresses and the peer gave up (see "Quick Share advertisement" above). The fix landed, but the end-to-end confirmation — a file arriving from the OnePlus 13 — has not been re-run since. That is the next thing to do.
3. **MTA sending needs a one-time Linux helper install.** Android receivers only join a real Wi-Fi Direct group owner, and wpa_supplicant's D-Bus API is root-only. The privileged operations now live in `satella-mta-helper` plus a packaged polkit action, so the CLI stays unprivileged and keeps `~/.config/satella/mta-identity`. Without that install, MTA send still needs the whole CLI under `sudo`/`pkexec`, or it falls back to the Satella-only NetworkManager hotspot. Distro packaging for the helper/policy files is still manual.
4. **mDNS/LocalSend error spam while `wlan0` is a GO.** Sockets still point at the previous address and log “Network is unreachable”. Cosmetic; suppress or rebind while the GO is up.
5. **NetworkManager keeps a stale `p2p-dev-wlan0` device.** The kernel interface is gone after the group is destroyed, but NM still lists the P2P device object. It does not block the Wi-Fi restore, which is re-activated explicitly. Cosmetic.
6. **Windows MTA** needs a WinRT BLE implementation, not vendored. LocalSend and Quick Share paths work there.
7. **Android specifics** — BLE permissions and a foreground service for background transfers — belong to the frontend shell, not the library. No platform-specific code exists in the core, by design.
8. **One global Tokio runtime** at the UniFFI boundary (noted with a `ponytail:` comment): all adapters share one worker pool, so a busy protocol could starve another. Acceptable until Android background-transfer load proves otherwise; then per-adapter executors.
9. **The daemon cannot be told anything while it runs.** `status` reads a state file, it does not talk to the process: there is no way to send a file, change the download directory or ask for a rescan from another process. There is no control socket and no `systemd` unit. The daemon also does not restart itself after a crash; `start` refuses while a pid file names a live process, and a dead one is simply overwritten.
10. **The daemon's log is not rotated.** `~/.cache/satella-cli/satella-cli.log` grows until the user removes it.
11. **The MTA refusal path has not been exercised against real hardware.** The vendor's callback is async and the answer channel works, but the only MTA receiver seen so far is a phone that sends, not one that is refused. The refusal needs a sender that waits long enough to be told "no".
12. **The MTA teardown does not survive a crash.** The credentials needed to tear a Wi-Fi Direct group down live only in the sending process's memory, so a process that dies leaves the group owner address on the interface and `dnsmasq` running. That is what produced the two-address advertisement above. The fix is to keep the record on disk; it has not been written yet.

---

## Documentation

- `README.md` — scope, crate layout, UniFFI contract, CLI usage, platform notes, license origins.
- `docs/designs/satella-init.md` — the finalized design: why one trait instead of four, why the protocol lives on the peer, why GPL-3.0 is forced, the verification strategy, and the four bugs the init uncovered.
- `crates/satella-mta/THIRD_PARTY_NOTICES.md` — fork provenance plus every change made to the vendored MTA engine, for rebases.
- `crates/satella-mta/vendor/mta-core/src/wifi/p2p_go.rs` — module docs explain why D-Bus, why root, and why `NoGroupIface`; the backend trait isolates the privileged transport.
- `crates/satella-mta-helper/` — the fixed-purpose root helper and its JSON protocol.
- `packaging/` — the polkit action and permanent rule installed by the runbook.
- `crates/satella-mta/vendor/mta-core/tests/p2p_go_probe.rs` — executable form of the runbook above.
- `crates/satella-cli/src/daemon.rs` — the daemon: why it re-executes instead of forking, how the readiness handshake works, and how the pid file is checked.