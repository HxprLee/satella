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

The Rust sources under `vendor/rqs-core/src/` are upstream's, otherwise
unchanged.

## License compatibility

The upstream license is GPL-3.0, the same license Satella is distributed under.
This is the reason Satella is GPL-3.0-or-later: forking this code makes Satella
a derivative work of it. The upstream copyright headers and license text are
preserved, as the GPL requires.
