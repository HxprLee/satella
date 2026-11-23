# Satella — State of the Project

**Stage:** v0.1.0, greenfield init complete and verified, committed.

---

## Layout

| Crate | Role | Lines (src) |
|---|---|---|
| `satella-core` | Protocol-neutral types + the `ProtocolAdapter` trait. No I/O, no protocol logic. | 399 |
| `satella-runtime` | Runs adapters concurrently, merges events, routes sends by protocol. | 182 |
| `satella-localsend` | LocalSend v2 adapter (vendored engine, 560K). | 751 |
| `satella-quickshare` | Quick Share adapter (vendored engine, 336K). | 370 |
| `satella-mta` | MTA adapter (vendored engine, 244K). | 355 |
| `satella-api` | The public UniFFI surface (`satella.udl`). | 461 |
| `satella-cli` | Terminal harness: discover / send / receive. | 339 |

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
- Vendored upstream suites also run: **99 + 34 + 13 + 5** tests, all green.

**Live evidence on this machine:**
- Real file transfer end to end — `hello.txt` sent, received, content verified, receiver reporting `incoming transfer over LocalSend`.
- Two instances discovering each other, both listed.
- A real BLE MTA device (`4axI`) discovered alongside LocalSend peers in the same merged list.
- Real MTA receive end to end against a OnePlus 13: BLE handshake, join `DIRECT-…`, DHCP, WSS, download, save.
- Real MTA send end to end against the same phone: wpa_supplicant group owner on 5 GHz (`DIRECT-gu`), DHCP on 192.168.49.0/24, BLE `P2pInfo`, phone joined, WSS `sendRequest`, ZIP download, status OK. Group, DHCP and address cleaned up afterwards.

**Release artifacts built:** `libsatella.so` (cdylib UniFFI loads), `libsatella.a`, `libsatella.rlib`, plus generated `satella.uniffi.rs` scaffolding.

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
2. **MTA sending needs root on Linux.** Android receivers only join a real Wi-Fi Direct group owner, and wpa_supplicant's D-Bus API is root-only. The CLI must run under `sudo`/`pkexec` for MTA sends; LocalSend and Quick Share stay unelevated. A privileged helper is the likely packaging fix.
3. **Windows MTA** needs a WinRT BLE implementation, not vendored. LocalSend and Quick Share paths work there.
4. **Android specifics** — BLE permissions and a foreground service for background transfers — belong to the frontend shell, not the library. No platform-specific code exists in the core, by design.
5. **One global Tokio runtime** at the UniFFI boundary (noted with a `ponytail:` comment): all adapters share one worker pool, so a busy protocol could starve another. Acceptable until Android background-transfer load proves otherwise; then per-adapter executors.

---

## Documentation

- `README.md` — scope, crate layout, UniFFI contract, CLI usage, platform notes, license origins.
- `docs/designs/satella-init.md` — the finalized design: why one trait instead of four, why the protocol lives on the peer, why GPL-3.0 is forced, the verification strategy, and the four bugs the init uncovered.