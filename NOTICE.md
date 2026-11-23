# Notice

Querent is licensed under GPL-3.0-or-later (`LICENSE`).

## Copied or adapted code

Each file adapted from another project names the source and is listed here with its license.

| File | Source | License |
| --- | --- | --- |
| `crates/querent/themes/solitude.json` | Colors from [solitude.nvim](https://github.com/MoXcz/solitude.nvim) (`lua/solitude/palette.lua`, `lua/solitude/highlights.lua`), Copyright (c) 2026 Oscar Marquez | MIT |
| `crates/querent/themes/catppuccin.json` | Colors from [Catppuccin](https://github.com/catppuccin/catppuccin) (`palette.json`), Copyright (c) 2021 Catppuccin | MIT |
| `crates/querent/themes/solarized.json` | Colors from [Solarized](https://github.com/altercation/solarized), Copyright (c) 2011 Ethan Schoonover | MIT |

## Main dependencies

| Crate | License |
| --- | --- |
| gpui (`gpui-pre`) | Apache-2.0 |
| gpui-kit, gpui-component | Apache-2.0 |
| mssql-tds | MIT |
| azure_identity, azure_core | MIT |
| tokio | MIT |
| tokio-postgres, postgres-native-tls | MIT OR Apache-2.0 |
| native-tls | MIT OR Apache-2.0 |
| rusqlite, libsqlite3-sys | MIT |
| snowflake-connector-rs | MIT |
| tree-sitter, tree-sitter-sequel | MIT |
| lsp-types | MIT |
| sqlformat | MIT OR Apache-2.0 |
| keyring, keyring-core and its platform stores | MIT OR Apache-2.0 |
| reqwest, url, base64, sha2, getrandom | MIT OR Apache-2.0 |
| minisign-verify, zip | MIT |
| self-replace | Apache-2.0 |
| semver, flate2, tar | MIT OR Apache-2.0 |
| SQLite (bundled by libsqlite3-sys) | Public domain |

Run `cargo tree --format "{p} {l}"` for the full list.
