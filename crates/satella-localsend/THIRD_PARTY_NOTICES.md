# Third-party notices for `satella-localsend`

This crate contains code forked from [localsend/localsend].

| Field | Value |
| --- | --- |
| Upstream | <https://github.com/localsend/localsend> |
| Upstream commit | `26cc8f997aa3f7ad789c12500a8b7206cfbb3ddc` |
| Upstream license | Apache License 2.0 |
| Forked path | `vendor/localsend-core/` |
| Upstream license text | `vendor/LICENSE.localsend` |

## What was changed

Only the crate manifest was changed:

* The package was renamed from `localsend` to `satella-localsend-vendor`, so it
  cannot collide with the upstream crate on crates.io.
* The crate was detached from the upstream workspace, so it carries its own
  dependency list.
* The upstream examples and integration tests were dropped, because Satella
  does not use them.

The Rust sources under `vendor/localsend-core/src/` are upstream's, unchanged.

## License compatibility

The Apache License 2.0 is compatible with the GPL-3.0-or-later license Satella
is distributed under. The upstream copyright headers and license text are
preserved, as the Apache License 2.0 requires.
