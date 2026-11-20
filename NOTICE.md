# Notice

sql-editor is licensed under GPL-3.0-or-later (`LICENSE`).

## Copied or adapted code

Each file adapted from another project names the source and is listed here with its license.

| File | Source | License |
| --- | --- | --- |
| `crates/sql_editor/themes/solitude.json` | Colors from [solitude.nvim](https://github.com/MoXcz/solitude.nvim) (`lua/solitude/palette.lua`, `lua/solitude/highlights.lua`), Copyright (c) 2026 Oscar Marquez | MIT |

## Main dependencies

| Crate | License |
| --- | --- |
| gpui (`gpui-pre`) | Apache-2.0 |
| gpui-kit, gpui-component | Apache-2.0 |
| mssql-tds | MIT |
| azure_identity, azure_core | MIT |
| tokio | MIT |

Run `cargo tree --format "{p} {l}"` for the full list.
