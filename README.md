# sql-editor

A desktop SQL editor written in Rust on [gpui](https://www.gpui.rs). It runs queries against Microsoft Fabric Warehouse and Lakehouse SQL endpoints. `ROADMAP.md` lists what comes next.

## Run

Sign in once with the Azure CLI; the editor uses that session to get a token.

```sh
az login
cargo run --release
```

Add a connection with `+` in the Connections sidebar: give it a name and paste a SQL Server connection string with Entra ID authentication, for example:

```text
Server=tcp:xxxx.datawarehouse.fabric.microsoft.com,1433;Database=my_warehouse;Authentication=ActiveDirectoryInteractive
```

The URL form works too:

```text
sqlserver://<client_id>%40<tenant_id>:<secret>@xxxx.datawarehouse.fabric.microsoft.com:1433?database=my_warehouse&fedauth=ActiveDirectoryServicePrincipal
```

A bare server name also works; then type the warehouse or lakehouse name in the tab's database field.

| Authentication / fedauth | Sign-in |
| --- | --- |
| `ActiveDirectoryDefault`, `ActiveDirectoryInteractive` and other Entra methods | your `az login` session |
| `ActiveDirectoryServicePrincipal` | client ID and secret from the string; the user is `client_id@tenant_id` (`%40` in the URL form), or add `tenant id=...` |

SQL logins are not supported yet. Connections are saved in `~/.config/sql-editor/connections.toml`, readable only by you, because a service principal string contains its secret.

Click a connection to use it in the current tab and expand its warehouses and lakehouses, write SQL, and press `Ctrl+Enter`. Expand a database to browse its tables, views, procedures and functions; right-click for Select Top 1000 Rows, Script as SELECT or CREATE, and Copy name; double-click to insert a name at the cursor.

Without the UI:

```sh
cargo run -p sql_db --example fabric -- "<connection string>" "select top 10 * from sys.tables"
```

Keys:

| Key | Action |
| --- | --- |
| `Ctrl+Enter` | Run the selection, or the statement under the cursor |
| `Ctrl+Shift+Enter` | Run everything in the tab |
| `Ctrl+Shift+C` | Cancel the running query |
| `Ctrl+Shift+E` | Export the result as CSV |
| `Ctrl+T` / `Ctrl+W` | New tab / close tab |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |

Fabric object names are case-sensitive: `INFORMATION_SCHEMA.TABLES`, not `information_schema.tables`.

## Vim

Vim keys are on by default in the SQL editor; the status bar shows the mode and has a button to turn them off. `ROADMAP.md` (0.0.6) lists the supported commands. App shortcuts such as `Ctrl+Enter` work in every mode.

## Themes

Solitude is built in and is the default. Pick a theme from the status bar; the choice is saved in `~/.config/sql-editor/settings.toml`. To add a theme, drop a [gpui-kit theme file](https://github.com/longbridge/gpui-kit/tree/main/themes) into `~/.config/sql-editor/themes/`; it loads without a restart. A theme with the same name as a built-in one replaces it.

## Linux build dependencies

Wayland and X11 headers, `libxkbcommon`, `fontconfig`, OpenSSL and a Vulkan driver.

## License

GPL-3.0-or-later. See `LICENSE` and `NOTICE.md`.
