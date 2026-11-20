# sql-editor

A desktop SQL editor written in Rust on [gpui](https://www.gpui.rs). It runs queries against SQL Server and Microsoft Fabric Warehouse and Lakehouse SQL endpoints. `ROADMAP.md` lists what comes next.

## Run

```sh
cargo run --release
```

Add a connection with `+` in the Connections sidebar. Pick the driver (SQL Server or Microsoft Fabric), fill in the server, database and sign-in method, and the form asks only for that method's values:

| Sign-in | Fields |
| --- | --- |
| SQL Server login | user, password |
| Entra ID (az login session) | none; run `az login` first |
| Entra ID service principal | tenant ID, client ID, client secret |

SQL Server also has encryption (Mandatory, Optional, Strict) and "Trust the server certificate" for self-signed servers. Fabric always uses port 1433 and strict encryption.

To start from a connection string, paste it in the box at the top of the form and press Fill from string. For example:

```text
Server=db1,1433;Database=sales;User ID=report;Password=...;TrustServerCertificate=True
Server=tcp:xxxx.datawarehouse.fabric.microsoft.com,1433;Database=my_warehouse;Authentication=ActiveDirectoryInteractive
```

The URL form works too:

```text
sqlserver://<client_id>%40<tenant_id>:<secret>@xxxx.datawarehouse.fabric.microsoft.com:1433?database=my_warehouse&fedauth=ActiveDirectoryServicePrincipal
sqlserver://report:<password>@db1/SQLEXPRESS?database=sales&TrustServerCertificate=true
```

A bare server name also works; then type the database name in the tab's database field.

| Authentication / fedauth | Sign-in |
| --- | --- |
| none, with `User ID` and `Password`, or `SqlPassword` | SQL Server login |
| `ActiveDirectoryDefault`, `ActiveDirectoryInteractive` and other Entra methods | your `az login` session |
| `ActiveDirectoryServicePrincipal` | client ID and secret from the string; the user is `client_id@tenant_id` (`%40` in the URL form), or add `tenant id=...` |

Windows authentication (`Integrated Security`) is not supported yet.

| Setting | Values |
| --- | --- |
| `Server` | `host`, `host,port`, `host\instance` (asks SQL Server Browser for the port), `.` or `(local)` for this machine |
| `Encrypt` | `True`/`Mandatory` (default), `False`/`Optional`, `Strict` (TDS 8.0). Fabric always uses `Strict`. |
| `TrustServerCertificate` | `True` for servers with a self-signed certificate |
| `HostNameInCertificate` | the name the certificate carries, when it differs from the server name |

Connections are saved as fields in `~/.config/sql-editor/connections.toml`, readable only by you, because they hold passwords and client secrets.

Click a connection to use it in the current tab and expand its databases (warehouses and lakehouses on Fabric), write SQL, and press `Ctrl+Enter`. Expand a database to browse its tables, views, procedures and functions; right-click for Select Top 1000 Rows, Script as SELECT or CREATE, and Copy name; double-click to insert a name at the cursor.

Without the UI:

```sh
cargo run -p sql_db --example query -- "<connection string>" "select top 10 * from sys.tables"
```

Keys:

| Key | Action |
| --- | --- |
| `Ctrl+Enter` | Run the selection, or the statement under the cursor |
| `Ctrl+Shift+Enter` | Run everything in the tab |
| `Ctrl+Shift+C` | Cancel the running query |
| `Ctrl+Shift+E` | Export the result as CSV |
| `Ctrl+N` / `Ctrl+Shift+W` | New tab / close tab |
| `Ctrl+W` | Delete the word before the cursor (vim insert mode, or with vim off) |
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
