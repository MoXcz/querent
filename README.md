# Querent

A desktop SQL editor written in Rust on [gpui](https://www.gpui.rs). It runs queries against SQL Server, Microsoft Fabric Warehouse and Lakehouse SQL endpoints, PostgreSQL, SQLite and Snowflake.

## Install

Download a release for Linux (`.tar.gz`) or Windows (`.zip` with `querent.exe`) from the GitHub releases page, or build from source:

```sh
cargo run --release
```

On Linux, run `scripts/install-linux.sh` from a clone of the repo to install the latest release into `~/.local/bin` with a desktop entry for app launchers. It checks the download's checksum and minisign signature, so it needs `minisign` or `rsign`. It also uninstalls `~/.cargo/bin/querent` if cargo installed it there.

A release copy offers to update itself when a new version is out ([Settings](docs/settings.md#updates)).

On Linux the build needs Wayland and X11 headers, `libxkbcommon`, `fontconfig`, OpenSSL and a Vulkan driver.

## First query

1. Press `+` in the Connections sidebar.
2. Pick the driver, fill in the server and database, and choose a sign-in method. Or paste a connection string and press Fill from string.
3. Press Test connection, then save.
4. Click the connection, write SQL, and press `Ctrl+Enter`.

```text
Server=db1,1433;Database=sales;User ID=report;Password=...;TrustServerCertificate=True
postgres://report:<password>@db1:5432/sales?sslmode=require
sqlite:///home/me/data/shop.db
```

Fabric object names are case-sensitive: `INFORMATION_SCHEMA.TABLES`, not `information_schema.tables`.

## Keys

| Key | Action |
| --- | --- |
| `Ctrl+Enter` | Run the selection, or the statement under the cursor, up to `;` or `GO` ([or a blank line](docs/settings.md#running-statements)) |
| `Ctrl+Shift+Enter` | Run everything in the tab |
| `Ctrl+Shift+C` | Cancel the running query; in the results grid, copy the selected cells with column names |
| `Ctrl+C` in the results grid | Copy the selected cells, ready to paste into Excel or Sheets |
| `Ctrl+Shift+E` | Export the result as CSV |
| `Ctrl+Shift+F` | Format the selection, or the whole tab |
| `Ctrl+Shift+P` | Command palette: every command, theme and setting by name |
| `Ctrl+,` | Settings |
| `Ctrl+Shift+H` | Query history for the current connection |
| `Ctrl+N` / `Ctrl+Shift+W` | New tab / close tab |
| `Ctrl+Shift+T` | Reopen the tab closed last |
| `Ctrl+O` | Open `.sql` files |
| `Ctrl+S` / `Ctrl+Shift+S` | Save / save as |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |
| `Ctrl+N` / `Ctrl+P` in a menu | Next / previous item |
| `Ctrl+C` / `Ctrl+X` / `Ctrl+V` | Copy / cut / paste; with nothing selected and vim mode off, copy or cut the whole line, and paste it above the current line |
| `Ctrl+W` | Delete the word before the cursor |

Vim keys are on by default; turn them off in Settings or with "Vim mode" in the command palette.

## Docs

| Page | Covers |
| --- | --- |
| [Connections](docs/connections.md) | sign-in methods, connection strings, encryption, Production and Read-only, where secrets are stored |
| [Editor](docs/editor.md) | the explorer, results grid, completion, files, session and history, running without the UI |
| [dbt](docs/dbt.md) | the dbt view: models, lineage, compiled SQL, previews |
| [Iceberg](docs/iceberg.md) | Iceberg REST catalogs: tables, snapshots, manifests and data files |
| [Settings](docs/settings.md) | themes, language servers, config files |
| [Vim](docs/vim.md) | supported modes, motions and operators |

## License

GPL-3.0-or-later. See `LICENSE` and `NOTICE.md`.
