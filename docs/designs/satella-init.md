# Satella init design

This is the finalized design Satella was initialized from. It records the
decisions behind the workspace layout, the adapter seam, the licensing, and
the two behaviours the whole design exists for: one merged device list, and a
pending receive that names the sender's protocol.

## Context

Satella started as an empty directory. The goal was one Rust library that
speaks LocalSend, Google Quick Share and the Mutual Transfer Alliance at the
same time, and that a frontend consumes as a single endpoint.

Three upstream projects supply the protocol engines:

| Protocol | Upstream | License |
| --- | --- | --- |
| LocalSend v2 | [localsend/localsend](https://github.com/localsend/localsend) | Apache-2.0 |
| Quick Share | [Martichou/rquickshare](https://github.com/Martichou/rquickshare) | GPL-3.0 |
| MTA | [Tinnci/cattysend](https://github.com/Tinnci/cattysend) | MIT |

The engines are forked, not reimplemented. Reimplementing three wire
protocols would be a large amount of code that would never be as correct as
the originals, and the interoperability risk would be enormous.

## Decisions

### 1. A protocol is an adapter, not a mode

The obvious design is one engine with a `Protocol` enum and a `match` in
every function. That design does not scale: adding a protocol means touching
every file.

Satella instead has exactly one trait, `ProtocolAdapter`, in `satella-core`:

```rust
#[async_trait]
pub trait ProtocolAdapter: Send + Sync + 'static {
    fn protocol(&self) -> Protocol;
    async fn start(&self, events: EventSender) -> AdapterResult<()>;
    async fn stop(&self) -> AdapterResult<()>;
    async fn announce(&self) -> AdapterResult<()> { Ok(()) }
    fn peers(&self) -> Vec<Peer>;
    async fn send(&self, request: SendRequest) -> AdapterResult<()>;
    async fn accept(&self, pending: ReceivePending, destination: PathBuf) -> AdapterResult<()>;
    async fn reject(&self, pending: ReceivePending) -> AdapterResult<()>;
}
```

The runtime holds `Vec<Arc<dyn ProtocolAdapter>>` and knows nothing else.
Adding a protocol means writing one crate that implements this trait and
adding it to a list. The core and the runtime do not change.

The trait is deliberately *one* trait rather than one per capability
(`discover`, `announce`, `send`, `receive`). Splitting it would force every
adapter to implement methods it cannot support, and every caller to
negotiate which combination of traits a given object has.

`announce` is the one method with a default, because only some protocols
announce: LocalSend answers multicast announcements with a register request,
so a device that never announces itself is invisible. Quick Share and MTA
are discovered passively and need nothing.

### 2. The protocol is part of the peer, not a property of the channel

Every `Peer` carries a `Protocol`:

```rust
pub struct Peer {
    pub protocol: Protocol,
    pub alias: String,
    pub fingerprint: String,
    // ...
}
```

The same physical device may be reachable over more than one protocol. If
the protocol lived outside the peer — in the adapter that produced it, say —
the merged list could not say how to reach a device, and a frontend could
not badge a row.

The same applies to `ReceivePending`:

```rust
pub struct ReceivePending {
    pub protocol: Protocol,     // the protocol the sender used
    pub transfer_id: String,
    pub sender: Peer,
    pub files: Vec<FileEntry>,
    pub total_size: u64,
}
```

This is the second of the two behaviours the design exists for. A frontend
that receives a pending transfer must be able to say "incoming via LocalSend"
before the user decides, and must route the accept to the right adapter. The
protocol travels with the transfer, so the frontend does not have to guess.

### 3. One runtime, merged events

`satella-runtime` starts every adapter, gives each its own event channel,
and forwards all of them into one. The runtime owns the merged peer list and
routes a send:

```rust
pub async fn send(&self, request: SendRequest) -> Result<()> {
    let adapter = self.adapters.iter()
        .find(|a| a.protocol() == request.peer.protocol)
        // ...
}
```

Routing by `peer.protocol` is the whole send path. The frontend never picks
an adapter; it picks a peer, and the peer knows its protocol.

One failing adapter does not stop the others. `Runtime::start` returns the
failures as a list and keeps the working adapters running, because a missing
Bluetooth radio must not take the whole app down.

### 4. The public surface is one UniFFI library

Only `satella-api` is public. The adapters are not exposed, so a frontend
cannot depend on a protocol type, and a protocol change cannot break a
frontend.

The contract is written once, in `satella.udl`:

```
dictionary Peer {
    Protocol protocol;
    string alias;
    string fingerprint;
    // ...
};

dictionary ReceivePending {
    Protocol protocol;
    string transfer_id;
    Peer sender;
    sequence<FileEntry> files;
    u64 total_size;
};
```

The UniFFI boundary is synchronous, so `satella-api` owns one global Tokio
runtime handle.

> `ponytail:` One global Tokio runtime for the whole FFI boundary. The
> ceiling is that every adapter shares the runtime's worker pool, so a busy
> protocol can starve another. This is acceptable until the Android
> background-transfer load proves otherwise; then move to per-adapter
> executors.

### 5. GPL-3.0-or-later, and why

The Quick Share engine is forked from `rquickshare`, which is GPL-3.0. A
fork is a derivative work, so Satella must be GPL-3.0-or-later. This is not
a preference; it is the license the derivation requires.

The Apache-2.0 LocalSend code and the MIT cattysend code are compatible as
inbound into GPL-3.0, so the choice does not conflict with them. Their
copyright headers, license texts and notices are preserved, and each adapter
crate carries a `THIRD_PARTY_NOTICES.md` naming the upstream project and the
exact commit it was forked from.

Only the protocol engines are forked. No application code is copied — in
particular, LocalSend's Flutter front end is not — and no upstream code is
relicensed.

## Verification

The gates that must pass:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
```

The runnable checks:

- `crates/satella-runtime/tests/merged_discovery.rs` — the merged list covers
  every protocol and every row names its protocol; a pending receive names
  the protocol its sender used; a send reaches the adapter that owns the
  peer's protocol.
- `crates/satella-localsend/tests/discovery.rs` — two real LocalSend adapters
  in one process: one announces itself over the multicast group, the other
  finds it through the HTTP register request. This is the real protocol path,
  not a stub.
- `cargo run -p satella-cli -- discover` — the merged list, every row labelled
  with its protocol, all three adapters running at once.

## What the init found

Three defects surfaced while making the checks pass, all in the adapter seam
rather than in the design:

1. **The discovery died immediately.** The LocalSend adapter created the
   discovery's stop sender and dropped it on the same line
   (`let (_discovery_stop_tx, discovery_stop_rx) = ...`). The vendor stops
   the discovery when that sender is dropped, so the adapter was announcing
   into a closed socket. The sender now lives in the adapter's state.

2. **Devices never announced themselves.** The adapter had no announcement
   at all. Without it, a Satella device is invisible to every LocalSend peer.
   `announce` is now part of the adapter trait and the runtime calls it for
   every adapter when it starts.

3. **Inbound registrations were dropped.** `ServerEventV2::Register` — the
   event that fires when a peer registers with our HTTP server, and the thing
   that confirms it as a peer — was ignored by a catch-all arm. It now feeds
   the discovery store.

A fourth, smaller one: the adapter's HTTP server took the fixed port
`53317`, so two Satella instances on one machine could not both run. It now
takes a free port and announces the port it actually bound, which is what the
protocol asks for.
