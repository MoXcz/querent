# Querent

A desktop SQL editor written in Rust on [gpui](https://www.gpui.rs). It runs queries against SQL Server, Microsoft Fabric Warehouse and Lakehouse SQL endpoints, PostgreSQL, SQLite and Snowflake.

## Run

```sh
cargo run --release
```

Release builds for Linux (`.tar.gz`) and Windows (`.zip` with `querent.exe`) are attached to each GitHub release.

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

SQL Server also has encryption (Mandatory, Optional, Strict) and PostgreSQL has Require (the default) or Prefer, which connects without encryption when the server has none; both have "Trust the server certificate" for self-signed servers. Fabric always uses port 1433 and strict encryption. PostgreSQL may leave the database empty to use the user's default database.

Test connection, at the bottom of the form, connects with the fields as they are and reports the result without saving. Tick Production to show a red PRODUCTION label in every tab that uses the connection. Tick Read-only to make its tabs refuse a run that contains a word that changes data (`insert`, `update`, `delete`, `into`, `exec`, `create`, `drop` and the like, outside strings, comments and quoted names); `select ... for update` and `select ... into` are refused too. Read-only guards against slips and is not a permission: a function that a `select` calls can still write, so use a database login without write rights where that matters.

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

A bare server name also works; then pick the database in the tab's database picker.

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

Connections are saved as fields in `~/.config/querent/connections.toml`. Passwords, client secrets, key passphrases and Entra sign-ins go to the OS keychain (Keychain on macOS, Credential Manager on Windows, a Secret Service such as GNOME Keyring or KeePassXC on Linux). Without a keychain they go to `~/.config/querent/secrets.toml`, readable only by you. Secrets that older versions wrote into `connections.toml` move out on the first start. Config from before the rename, in `~/.config/sql-editor`, moves to `~/.config/querent` on the first start; a file that exists in both stays in the old folder for you to merge by hand.

Click a connection to use it in the current tab and expand its databases (warehouses and lakehouses on Fabric), write SQL, and press `Ctrl+Enter`. The tab's database picker lists the same databases; type to filter it, or type a name that is not listed and pick "Use …". Expand a database to browse its tables, views, procedures and functions; right-click for Select Top 1000 Rows, Script as SELECT or CREATE, and Copy name; double-click to insert a name at the cursor. On SQL Server and PostgreSQL an expanded table lists its keys, checks and indexes under its columns, and its CREATE script includes them with defaults, identity and computed columns; a default constraint's own name, triggers, partitioning, and XML, spatial and hash indexes are left out, and a PostgreSQL `serial` column is written as an identity column.

Results show above the grid a button per result set, Messages, and Export CSV and JSON. Type in "Filter rows" under the grid to keep only the rows with a value that contains the text; the exports and "Copy all rows" write the rows the grid shows, in its order. The status line starts with "Open transaction" when a run on SQL Server, PostgreSQL or SQLite left one open on the tab's connection; closing that tab or the window then asks first, because the server rolls the transaction back. Click a column's sort arrow to order the rows by it (NULL first, numbers by value); right-click a row to copy it, with or without the column names, to copy every row, or to view its full values, which the grid cuts at 500 characters; each value there has a Copy button for the whole value. Copied rows are tab-separated, so they paste into a spreadsheet. When the server reports where a statement failed, the error starts with the tab's line number and a button jumps to that line. SQL Server and PostgreSQL report a line for most errors, SQLite and Snowflake for syntax errors. With a row limit set (Settings), a single read-only statement stops on the server once the limit is reached; a script with several statements, a statement calling a function that is not a common built-in (it might write), or a run inside an open transaction still reads to the end and only keeps the first rows, so nothing it would write is cut short.

While typing, a menu offers tables after `from` and `join`, the columns of `alias.`, and keywords; hover over a table or column to see its columns or type. The tables and columns are read once per database in the background. PostgreSQL, SQLite and Snowflake tabs also underline syntax errors.

### Language servers

A language server adds its completions, hover and diagnostics to the built-in ones, and can format SQL instead of the built-in formatter ("Use the language server" in Settings). Choose one per database kind (`sql_server`, `fabric`, `postgres`, `sqlite`, `snowflake`) in `~/.config/querent/settings.toml`:

```toml
[language_servers.postgres]
command = "sqls"
pass_connection = true   # send the connection, including its password, to the server
```

The status line shows the server's state; a server that stops restarts on next use.

Two more commands live in the command palette: "Refresh schema" reads a database's tables and columns again for completion, and "Sign out of Entra ID" forgets a connection's browser sign-in so the next run asks again.

A tab can hold a `.sql` file: its title is the file name, with `*` while it has changes that are not saved, and closing it then asks first. Files are read as UTF-8, as UTF-16 when they start with a byte order mark (as SQL Server Management Studio writes them), or as Windows-1252 when they are not valid UTF-8. They are always saved as UTF-8. Dropping files on the window opens them too. When another program changes a tab's file, the tab takes the new text the next time you switch to it or back to the app; a tab with changes of its own keeps them and warns that saving will overwrite the file.

Tabs and their text come back after a restart (`~/.config/querent/session.json`), and every run is kept in `history.jsonl` next to it, except statements that look like they set a password (the tab's own text is still saved with the session); "Clear query history" in the command palette empties it. Both files are readable only by you, since queries can hold sensitive values.

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
| `Ctrl+O` | Open `.sql` files, each in a tab |
| `Ctrl+S` / `Ctrl+Shift+S` | Save the tab to its file / save to a new file |
| `Ctrl+Shift+T` | Reopen the tab closed last |
| `Ctrl+N` / `Ctrl+P` with the completion menu, command palette or query history open | Next / previous item (`Ctrl+N` opens a tab only when no menu is open) |
| `Ctrl+W` | Delete the word before the cursor (vim insert mode, or with vim off) |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |

Fabric object names are case-sensitive: `INFORMATION_SCHEMA.TABLES`, not `information_schema.tables`.

## Vim

Vim keys are on by default in the SQL editor; the status bar shows the mode. Turn them off in Settings (`Ctrl+,`) or with "Vim mode" in the command palette. App shortcuts such as `Ctrl+Enter` work in every mode.

Supported:

- Modes: normal, insert, visual, visual line.
- Motions: `h j k l w W b B e E 0 ^ $ gg G {count}G f F t T ; , { } n N ctrl-d ctrl-u`, Enter, Space, Backspace.
- Operators `d c y` with motions, counts (`2d3w`) and text objects `iw aw iW aW i( a( ib i[ i{ iB i< i' i"` and ``i` ``; `dd cc yy`; `x X D C s S Y p P J ~ r`.
- `i a I A o O`, `u`, `ctrl-r`, `.`, `/` search with smartcase, `*`.

## Themes

Solitude (the default), Catppuccin (Latte, Frappé, Macchiato, Mocha) and Solarized (Dark, Light) are built in. Pick a theme in Settings (`Ctrl+,`) or type "Theme" in the command palette (`Ctrl+Shift+P`); the choice is saved in `~/.config/querent/settings.toml`. To add a theme, drop a [gpui-kit theme file](https://github.com/longbridge/gpui-kit/tree/main/themes) into `~/.config/querent/themes/`; it loads without a restart. A theme with the same name as a built-in one replaces it.

## Linux build dependencies

Wayland and X11 headers, `libxkbcommon`, `fontconfig`, OpenSSL and a Vulkan driver.

## License

GPL-3.0-or-later. See `LICENSE` and `NOTICE.md`.
