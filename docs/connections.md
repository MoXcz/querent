# Connections

Add a connection with `+` in the Connections sidebar. Pick the driver (SQL Server, Microsoft Fabric, PostgreSQL, SQLite or Snowflake), fill in the server and database, and choose a sign-in method. The form then asks only for that method's values.

## Sign-in methods

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

PostgreSQL may leave the database empty to use the user's default database.

### Entra browser sign-in

The first connect opens your browser and shows a notification until you finish. Later connections, and later starts, reuse the sign-in until Entra asks again.

With a device code, the code is copied to the clipboard and the notification links to the page where you enter it. `Ctrl+Shift+C` stops a sign-in that a tab is waiting on.

"Sign out of Entra ID" in the command palette forgets a connection's browser sign-in, so the next run asks again.

## Encryption

| Driver | Options |
| --- | --- |
| SQL Server | Mandatory, Optional, Strict |
| PostgreSQL | Require (default), or Prefer, which connects without encryption when the server has none |
| Fabric | always port 1433 and Strict |

SQL Server and PostgreSQL both have "Trust the server certificate" for self-signed servers.

## Form options

- **Test connection** connects with the fields as they are and reports the result without saving.
- **Production** shows a red PRODUCTION label in every tab that uses the connection.
- **Read-only** makes the connection's tabs refuse a run that changes data. See below.

### Read-only

A tab refuses a run that contains a word that changes data, such as `insert`, `update`, `delete`, `into`, `exec`, `create` or `drop`. Words inside strings, comments and quoted names do not count. `select ... for update` and `select ... into` are refused too.

Read-only guards against slips and is not a permission: a function that a `select` calls can still write. Where that matters, use a database login without write rights.

## Connection strings

Paste a string in the box at the top of the form and press Fill from string.

```text
Server=db1,1433;Database=sales;User ID=report;Password=...;TrustServerCertificate=True
Server=tcp:xxxx.datawarehouse.fabric.microsoft.com,1433;Database=my_warehouse;Authentication=ActiveDirectoryInteractive
postgres://report:<password>@db1:5432/sales?sslmode=require
host=db1 port=5432 user=report dbname=sales
sqlite:///home/me/data/shop.db
snowflake://report:<password>@myorg-myaccount/SALES/PUBLIC?warehouse=REPORTING&role=ANALYST
```

SQL Server also takes the URL form:

```text
sqlserver://<client_id>%40<tenant_id>:<secret>@xxxx.datawarehouse.fabric.microsoft.com:1433?database=my_warehouse&fedauth=ActiveDirectoryServicePrincipal
sqlserver://report:<password>@db1/SQLEXPRESS?database=sales&TrustServerCertificate=true
```

A bare server name also works; then pick the database in the tab's database picker.

### SQL Server `Authentication` / `fedauth`

| Value | Sign-in |
| --- | --- |
| none, with `User ID` and `Password`, or `SqlPassword` | SQL Server login |
| `ActiveDirectoryInteractive` | Entra browser sign-in; `User ID` preselects the account |
| `ActiveDirectoryDeviceCodeFlow` | Entra sign-in with a device code |
| `ActiveDirectoryDefault`, `ActiveDirectoryIntegrated`, `ActiveDirectoryManagedIdentity` | your `az login` session |
| `Integrated Security=SSPI` or `Trusted_Connection=yes` | Windows authentication |
| `ActiveDirectoryServicePrincipal` | client ID and secret from the string; the user is `client_id@tenant_id` (`%40` in the URL form), or add `tenant id=...` |

### SQL Server settings

| Setting | Values |
| --- | --- |
| `Server` | `host`, `host,port`, `host\instance` (asks SQL Server Browser for the port), `.` or `(local)` for this machine |
| `Encrypt` | `True`/`Mandatory` (default), `False`/`Optional`, `Strict` (TDS 8.0). Fabric always uses `Strict`. |
| `TrustServerCertificate` | `True` for servers with a self-signed certificate |
| `HostNameInCertificate` | the name the certificate carries, when it differs from the server name |

## Where connections are stored

Connection fields go to `~/.config/querent/connections.toml`. Passwords, client secrets, key passphrases and Entra sign-ins go to the OS keychain:

| OS | Keychain |
| --- | --- |
| macOS | Keychain |
| Windows | Credential Manager |
| Linux | a Secret Service such as GNOME Keyring or KeePassXC |

Without a keychain, secrets go to `~/.config/querent/secrets.toml`, readable only by you.

On the first start, secrets that older versions wrote into `connections.toml` move out. Config in `~/.config/sql-editor`, from before the rename, moves to `~/.config/querent`; a file that exists in both stays in the old folder for you to merge by hand.
