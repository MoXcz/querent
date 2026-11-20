# sql-editor

A desktop SQL editor written in Rust on [gpui](https://www.gpui.rs). It runs queries against SQL Server, Microsoft Fabric Warehouse and Lakehouse SQL endpoints, PostgreSQL, SQLite and Snowflake. `ROADMAP.md` lists what comes next.

## Run

```sh
cargo run --release
```

Add a connection with `+` in the Connections sidebar. Pick the driver (SQL Server, Microsoft Fabric, PostgreSQL, SQLite or Snowflake), fill in the server, database and sign-in method, and the form asks only for that method's values:

| Sign-in | Fields |
| --- | --- |
| SQL Server login, or user name and password (PostgreSQL) | user, password |
| Entra ID (az login session) | none; run `az login` first |
| Entra ID (browser sign-in) | all optional: user to preselect, tenant ID, client ID, and "Sign in with a code on another device" |
| Windows authentication (SQL Server) | none; uses your Windows sign-in, or on Linux and macOS a Kerberos ticket from `kinit` |
| Entra ID service principal | tenant ID, client ID, client secret |
| None (SQLite) | the database file, typed or chosen with Browse… |
| Key pair (Snowflake) | user, private key file (`.p8`), passphrase if the key is encrypted |

Snowflake also takes an account identifier (`orgname-accountname` or `xy12345.us-east-1`) and an optional warehouse, role and schema.

SQL Server also has encryption (Mandatory, Optional, Strict) and PostgreSQL has Prefer or Require; both have "Trust the server certificate" for self-signed servers. Fabric always uses port 1433 and strict encryption. PostgreSQL may leave the database empty to use the user's default database.

To start from a connection string, paste it in the box at the top of the form and press Fill from string. For example:

```text
Server=db1,1433;Database=sales;User ID=report;Password=...;TrustServerCertificate=True
Server=tcp:xxxx.datawarehouse.fabric.microsoft.com,1433;Database=my_warehouse;Authentication=ActiveDirectoryInteractive
postgres://report:<password>@db1:5432/sales?sslmode=require
host=db1 port=5432 user=report dbname=sales
sqlite:///home/me/data/shop.db
snowflake://report:<password>@myorg-myaccount/SALES/PUBLIC?warehouse=REPORTING&role=ANALYST
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
| `ActiveDirectoryInteractive` | Entra browser sign-in; `User ID` preselects the account |
| `ActiveDirectoryDeviceCodeFlow` | Entra sign-in with a device code |
| `ActiveDirectoryDefault`, `ActiveDirectoryIntegrated`, `ActiveDirectoryManagedIdentity` | your `az login` session |
| `Integrated Security=SSPI` or `Trusted_Connection=yes` | Windows authentication |
| `ActiveDirectoryServicePrincipal` | client ID and secret from the string; the user is `client_id@tenant_id` (`%40` in the URL form), or add `tenant id=...` |

| Setting | Values |
| --- | --- |
| `Server` | `host`, `host,port`, `host\instance` (asks SQL Server Browser for the port), `.` or `(local)` for this machine |
| `Encrypt` | `True`/`Mandatory` (default), `False`/`Optional`, `Strict` (TDS 8.0). Fabric always uses `Strict`. |
| `TrustServerCertificate` | `True` for servers with a self-signed certificate |
| `HostNameInCertificate` | the name the certificate carries, when it differs from the server name |

Browser sign-in opens your browser on the first connect and shows a notification until you finish; later connections, and later starts, reuse the sign-in until Entra asks again. With a device code, the code is copied to the clipboard and the notification links to the page where you enter it. `Ctrl+Shift+C` stops a sign-in that a tab is waiting on.

Connections are saved as fields in `~/.config/sql-editor/connections.toml`. Passwords, client secrets, key passphrases and Entra sign-ins go to the OS keychain (Keychain on macOS, Credential Manager on Windows, a Secret Service such as GNOME Keyring or KeePassXC on Linux). Without a keychain they go to `~/.config/sql-editor/secrets.toml`, readable only by you. Secrets that older versions wrote into `connections.toml` move out on the first start.

Click a connection to use it in the current tab and expand its databases (warehouses and lakehouses on Fabric), write SQL, and press `Ctrl+Enter`. Expand a database to browse its tables, views, procedures and functions; right-click for Select Top 1000 Rows, Script as SELECT or CREATE, and Copy name; double-click to insert a name at the cursor.

While typing, a menu offers tables after `from` and `join`, the columns of `alias.`, and keywords; hover over a table or column to see its columns or type. The tables and columns are read once per database in the background. PostgreSQL, SQLite and Snowflake tabs also underline syntax errors.

### Language servers

A language server adds its completions, hover and diagnostics to the built-in ones, and can format SQL instead of the built-in formatter ("Use the language server" in Settings). Choose one per database kind (`sql_server`, `fabric`, `postgres`, `sqlite`, `snowflake`) in `~/.config/sql-editor/settings.toml`:

```toml
[language_servers.postgres]
command = "sqls"
pass_connection = true   # send the connection, including its password, to the server
```

The status line shows the server's state; a server that stops restarts on next use.

Tabs and their text come back after a restart (`~/.config/sql-editor/session.json`), and every run is kept in `history.jsonl` next to it. Both files are readable only by you, since queries can hold sensitive values.

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
| `Ctrl+Shift+F` | Format the selection, or the whole tab (style in Settings) |
| `Ctrl+Shift+P` | Command palette: every command, theme and setting by name |
| `Ctrl+,` | Settings (theme, vim mode, explorer grouping); also the gear in the status bar |
| `Ctrl+Shift+H` | Query history for the current connection |
| `Ctrl+N` / `Ctrl+Shift+W` | New tab / close tab |
| `Ctrl+N` / `Ctrl+P` with the completion menu open | Next / previous suggestion (`Ctrl+N` opens a tab only when no menu is open) |
| `Ctrl+W` | Delete the word before the cursor (vim insert mode, or with vim off) |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |

Fabric object names are case-sensitive: `INFORMATION_SCHEMA.TABLES`, not `information_schema.tables`.

## Vim

Vim keys are on by default in the SQL editor; the status bar shows the mode. Turn them off in Settings (`Ctrl+,`) or with "Vim mode" in the command palette. `ROADMAP.md` (0.0.6) lists the supported commands. App shortcuts such as `Ctrl+Enter` work in every mode.

## Themes

Solitude is built in and is the default. Pick a theme in Settings (`Ctrl+,`) or type "Theme" in the command palette (`Ctrl+Shift+P`); the choice is saved in `~/.config/sql-editor/settings.toml`. To add a theme, drop a [gpui-kit theme file](https://github.com/longbridge/gpui-kit/tree/main/themes) into `~/.config/sql-editor/themes/`; it loads without a restart. A theme with the same name as a built-in one replaces it.

## Linux build dependencies

Wayland and X11 headers, `libxkbcommon`, `fontconfig`, OpenSSL and a Vulkan driver.

## License

GPL-3.0-or-later. See `LICENSE` and `NOTICE.md`.
