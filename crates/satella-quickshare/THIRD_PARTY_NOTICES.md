# Third-party notices for `satella-quickshare`

This crate contains code forked from [Martichou/rquickshare].

| Field | Value |
| --- | --- |
| Upstream | <https://github.com/Martichou/rquickshare> |
| Upstream commit | `378d8ae969941bee4bf60ad34ac9cf8bb7005eb7` |
| Upstream license | GNU General Public License v3.0 |
| Forked path | `vendor/rqs-core/` |
| Upstream license text | `vendor/LICENSE.rquickshare` |

## What was changed

* The package was renamed from `rqs_lib` to `satella-quickshare-vendor`, so it
  cannot collide with the upstream crate.
* The JavaScript frontend's files (`bindings/`, `esm/`, `dist/`, `package.json`,
  `tsconfig.json`) were dropped, because Satella has no JavaScript frontend.
* The build script no longer writes the TypeScript binding index. The protobuf
  compilation is unchanged.
* `vendor/rqs-core/src/hdl/mdns.rs`: the advertised service names exactly one
  address now, the one of the default route's subnet, and `enable_addr_auto`
  is off. With automatic addressing the daemon swept in every address of the
  host, so a Wi-Fi Direct group owner left over from an interrupted MTA send
  was advertised next to the real one and the peer got two competing answers
  for the same service, one of them unroutable. The TXT record also carries
  the `IPv4` hint the phone publishes. The service is rebuilt and re-registered
  when the address to advertise changes, which keeps the behaviour of the
  dropped flag without the multi-address advertisement.
* `vendor/rqs-core/src/utils.rs`: `reachable_ipv4()` picks the address to
  advertise. It reads the default route from `/proc/net/route` and takes the
  address of that interface that is in the gateway's subnet.
* `vendor/rqs-core/src/manager.rs`: the accept of an inbound connection is
  logged at info level instead of at trace level. The daemon's log filter is
  `info`, so a connection that was accepted and then dropped left no record
  at all, which is what hid the failure.
* `vendor/rqs-core/src/hdl/inbound.rs` and `vendor/rqs-core/src/hdl/outbound.rs`:
  the encrypted frame handling now answers a `DISCONNECTION` frame instead of
  logging it as unhandled. A peer closes the session with that frame and
  then hangs up; the engine used to treat the frame as an unknown one and
  the socket ending as a failure, so a transfer that had worked was reported
  as a broken one. A transfer that already finished stays finished.
* `vendor/rqs-core/src/manager.rs`: a socket that ends after the session was
  over is logged at debug level instead of at error level. The end of the
  socket is the normal end of a transfer, and reporting it as an error buried
  the failures that matter.
* `vendor/rqs-core/src/hdl/session_tests.rs` (new file): the checks of the
  session teardown. They drive the real handlers over a real socket pair
  with the real encryption.
* `vendor/rqs-core/src/outcome.rs` (new file): the receipt of an outgoing
  transfer. The engine hands a send request over a channel, so the caller
  cannot watch the session it started. The engine therefore answers over a
  registry of one-shot receipts, keyed by the id of the transfer, and the
  receipt says what really happened: the peer agreed, refused, or never
  answered. The registry is used instead of a field inside `SendInfo`,
  because `SendInfo` is serializable and its TypeScript binding is generated
  from it, and a one-shot sender is neither.
* `vendor/rqs-core/src/manager.rs`: the server delivers the receipt of a
  transfer it just ran. The outcome is derived from the final state of the
  session, so a transfer that failed is never reported as one that worked.
* `vendor/rqs-core/src/hdl/outbound.rs`: a peer that refuses the transfer is
  recorded as `State::Rejected` instead of `State::Disconnected`. A refusal
  and a peer that hangs up are different outcomes, and the caller has to be
  able to tell them apart.
* `vendor/rqs-core/src/hdl/outbound.rs`: the wait for the peer is bounded.
  The engine used to read the socket without a deadline, so a peer that
  ignored the request kept the session pending forever and the caller never
  learned anything. The deadline covers the handshake and the consent only:
  once the files are on their way a slow peer is not a lost one.
* `vendor/rqs-core/src/hdl/outcome_tests.rs` (new file): the checks of the
  outcome of an outgoing transfer. They drive the real handlers over a real
  socket pair, with the real encryption.

Apart from the changes listed above, the Rust sources under
`vendor/rqs-core/src/` are upstream's, unchanged.

## License compatibility

The upstream license is GPL-3.0, the same license Satella is distributed under.
This is the reason Satella is GPL-3.0-or-later: forking this code makes Satella
a derivative work of it. The upstream copyright headers and license text are
preserved, as the GPL requires.
