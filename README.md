# sql-editor

A desktop SQL editor written in Rust on [gpui](https://www.gpui.rs). It runs queries against Microsoft Fabric Warehouse and Lakehouse SQL endpoints. `ROADMAP.md` lists what comes next.

## Run

Sign in once with the Azure CLI; the editor uses that session to get a token.

```sh
az login
cargo run --release
```

Fill in the SQL endpoint host (Fabric: workspace settings, or the item's "SQL connection string") and the warehouse or lakehouse name, write SQL, and press `Ctrl+Enter`.

To prefill the connection fields:

```sh
SQL_EDITOR_FABRIC_HOST=xxxx.datawarehouse.fabric.microsoft.com \
SQL_EDITOR_FABRIC_DATABASE=my_warehouse \
cargo run --release
```

Without the UI:

```sh
cargo run -p sql_db --example fabric -- <host> <database> "select top 10 * from sys.tables"
```

Keys: `Ctrl+Enter` runs the selection or the statement under the cursor, `Ctrl+Shift+Enter` runs everything, `Ctrl+Shift+C` cancels.

Fabric object names are case-sensitive: `INFORMATION_SCHEMA.TABLES`, not `information_schema.tables`.

## Themes

Solitude is built in and is the default. Pick a theme from the status bar; the choice is saved in `~/.config/sql-editor/settings.toml`. To add a theme, drop a [gpui-kit theme file](https://github.com/longbridge/gpui-kit/tree/main/themes) into `~/.config/sql-editor/themes/`; it loads without a restart. A theme with the same name as a built-in one replaces it.

## Linux build dependencies

Wayland and X11 headers, `libxkbcommon`, `fontconfig`, OpenSSL and a Vulkan driver.

## License

GPL-3.0-or-later. See `LICENSE` and `NOTICE.md`.
