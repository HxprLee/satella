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

The Rust sources under `vendor/mta-core/src/` are upstream's, unchanged.

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
