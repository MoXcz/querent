//! ADO.NET-style SQL Server connection strings, as SSMS, Azure Data Studio and
//! the Fabric portal produce them.

use crate::{Authentication, Encryption, TdsConfig};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectionStringError {
    #[error("the connection string has no Server")]
    MissingServer,
    #[error("invalid port in Server: {0}")]
    InvalidPort(String),
    #[error("the port field says {0} but the server name says {1}")]
    ConflictingPorts(u16, u16),
    #[error("unterminated quote in the value of {0}")]
    UnterminatedQuote(String),
    #[error("invalid value for {key}: {value}")]
    InvalidValue { key: &'static str, value: String },
    #[error(
        "Authentication={0} is not supported yet; use SqlPassword, ActiveDirectoryDefault, ActiveDirectoryInteractive or ActiveDirectoryServicePrincipal"
    )]
    UnsupportedAuthentication(String),
    #[error("a SQL login needs a User ID")]
    MissingUser,
    #[error("a service principal connection string needs a {0}")]
    ServicePrincipalField(&'static str),
}

/// Entra methods that can use the signed-in Azure CLI session's token.
const ENTRA_METHODS: [&str; 3] = [
    "activedirectorydefault",
    "activedirectoryintegrated",
    "activedirectorymanagedidentity",
];
const INTERACTIVE: &str = "activedirectoryinteractive";
const DEVICE_CODE: &str = "activedirectorydevicecodeflow";
const SERVICE_PRINCIPAL: &str = "activedirectoryserviceprincipal";
const SQL_PASSWORD: &str = "sqlpassword";

#[derive(Debug, Default)]
struct Fields {
    server: Option<String>,
    database: Option<String>,
    authentication: Option<String>,
    user: Option<String>,
    password: Option<String>,
    tenant: Option<String>,
    encrypt: Option<String>,
    trust_server_certificate: Option<String>,
    host_name_in_certificate: Option<String>,
    integrated_security: Option<String>,
}

impl Fields {
    fn set(&mut self, key: &str, value: String) {
        match normalize_key(key).as_str() {
            "server" | "datasource" | "address" | "addr" | "networkaddress" => {
                self.server = Some(value);
            }
            "database" | "initialcatalog" => self.database = Some(value),
            "authentication" | "fedauth" => self.authentication = Some(value),
            "userid" | "uid" | "user" => self.user = Some(value),
            "password" | "pwd" => self.password = Some(value),
            "tenantid" | "tenant" => self.tenant = Some(value),
            "encrypt" => self.encrypt = Some(value),
            "trustservercertificate" => self.trust_server_certificate = Some(value),
            "hostnameincertificate" | "hostnameincert" => {
                self.host_name_in_certificate = Some(value);
            }
            "integratedsecurity" | "trustedconnection" | "trusted_connection" => {
                self.integrated_security = Some(value);
            }
            _ => {}
        }
    }
}

/// Parses a connection string into a SQL Server or Fabric target.
///
/// Accepts ADO.NET form (`Server=...;Database=...`), URL form
/// (`sqlserver://user:password@host:port/instance?database=...&fedauth=...`), or a
/// bare server name, which is what the Fabric portal's copy button gives; the
/// database then comes from elsewhere, so it may be empty.
pub fn parse_connection_string(text: &str) -> Result<TdsConfig, ConnectionStringError> {
    let text = text.trim();
    let fields = if has_url_scheme(text) {
        url_fields(text)
    } else if text.contains('=') {
        let mut fields = Fields::default();
        for (key, value) in pairs(text)? {
            fields.set(&key, value);
        }
        fields
    } else {
        Fields {
            server: Some(text.to_owned()),
            ..Fields::default()
        }
    };
    build(fields)
}

fn build(fields: Fields) -> Result<TdsConfig, ConnectionStringError> {
    let authentication = authentication(&fields)?;
    let server = fields
        .server
        .as_deref()
        .filter(|server| !server.is_empty())
        .ok_or(ConnectionStringError::MissingServer)?;
    let server = parse_server(server)?;
    let encryption = match fields.encrypt.as_deref().map(normalize_key).as_deref() {
        None | Some("true" | "yes" | "mandatory") => Encryption::Mandatory,
        Some("false" | "no" | "optional") => Encryption::Optional,
        Some("strict") => Encryption::Strict,
        Some(_) => {
            return Err(ConnectionStringError::InvalidValue {
                key: "Encrypt",
                value: fields.encrypt.unwrap_or_default(),
            });
        }
    };
    let trust_server_certificate = fields
        .trust_server_certificate
        .as_deref()
        .map(|value| parse_bool("TrustServerCertificate", value))
        .transpose()?
        .unwrap_or(false);
    Ok(TdsConfig {
        port: server.port,
        instance: server.instance,
        authentication,
        encryption,
        trust_server_certificate,
        host_name_in_certificate: fields
            .host_name_in_certificate
            .filter(|name| !name.is_empty()),
        ..TdsConfig::new(server.host, fields.database.unwrap_or_default())
    })
}

fn authentication(fields: &Fields) -> Result<Authentication, ConnectionStringError> {
    let integrated = fields.integrated_security.as_deref().is_some_and(|value| {
        normalize_key(value) == "sspi" || parse_bool("Integrated Security", value) == Ok(true)
    });
    let has_user = fields.user.as_deref().is_some_and(|user| !user.is_empty());
    match fields.authentication.as_deref().map(normalize_key) {
        None if integrated => Ok(Authentication::Integrated),
        None if has_user => sql_login(fields),
        None => Ok(Authentication::AzureCli),
        Some(method) if method == SQL_PASSWORD => sql_login(fields),
        Some(method) if ENTRA_METHODS.contains(&method.as_str()) => Ok(Authentication::AzureCli),
        Some(method) if method == INTERACTIVE || method == DEVICE_CODE => {
            Ok(Authentication::EntraInteractive {
                tenant_id: fields.tenant.clone().unwrap_or_default(),
                user: fields.user.clone().unwrap_or_default(),
                client_id: String::new(),
                device_code: method == DEVICE_CODE,
            })
        }
        Some(method) if method == SERVICE_PRINCIPAL => service_principal(fields),
        Some(_) => Err(ConnectionStringError::UnsupportedAuthentication(
            fields.authentication.clone().unwrap_or_default(),
        )),
    }
}

fn sql_login(fields: &Fields) -> Result<Authentication, ConnectionStringError> {
    let user = fields
        .user
        .clone()
        .filter(|user| !user.is_empty())
        .ok_or(ConnectionStringError::MissingUser)?;
    Ok(Authentication::Password {
        user,
        password: fields.password.clone().unwrap_or_default(),
    })
}

fn parse_bool(key: &'static str, value: &str) -> Result<bool, ConnectionStringError> {
    match normalize_key(value).as_str() {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        _ => Err(ConnectionStringError::InvalidValue {
            key,
            value: value.to_owned(),
        }),
    }
}

/// The client ID is the user, as `client_id@tenant_id` or with a separate tenant field.
fn service_principal(fields: &Fields) -> Result<Authentication, ConnectionStringError> {
    let user = fields.user.clone().filter(|user| !user.is_empty()).ok_or(
        ConnectionStringError::ServicePrincipalField("client ID (the user)"),
    )?;
    let (client_id, tenant_id) = match (user.split_once('@'), &fields.tenant) {
        (_, Some(tenant)) if !tenant.is_empty() => (user.clone(), tenant.clone()),
        (Some((client, tenant)), _) => (client.to_owned(), tenant.to_owned()),
        (None, _) => {
            return Err(ConnectionStringError::ServicePrincipalField(
                "tenant ID (write the user as client_id@tenant_id)",
            ));
        }
    };
    let client_secret = fields
        .password
        .clone()
        .filter(|password| !password.is_empty())
        .ok_or(ConnectionStringError::ServicePrincipalField(
            "client secret (the password)",
        ))?;
    Ok(Authentication::ServicePrincipal {
        tenant_id,
        client_id,
        client_secret,
    })
}

fn has_url_scheme(text: &str) -> bool {
    text.get(..12)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("sqlserver://"))
}

/// `sqlserver://[user[:password]@]host[:port][/instance][?key=value&...]`, as go-mssqldb and ADBC use.
fn url_fields(text: &str) -> Fields {
    let rest = text.get(12..).unwrap_or_default();
    let (before_query, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, instance) = before_query.split_once('/').unwrap_or((before_query, ""));
    let instance = instance.trim_end_matches('/');
    let (user_info, host_port) = match authority.rsplit_once('@') {
        Some((user_info, host_port)) => (Some(user_info), host_port),
        None => (None, authority),
    };
    let mut fields = Fields::default();
    if let Some(user_info) = user_info {
        let (user, password) = match user_info.split_once(':') {
            Some((user, password)) => (user, Some(password)),
            None => (user_info, None),
        };
        fields.user = Some(percent_decode(user));
        fields.password = password.map(percent_decode);
    }
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (host_port, None),
    };
    let mut server = host.to_owned();
    if !instance.is_empty() {
        server = format!("{server}\\{instance}");
    }
    if let Some(port) = port {
        server = format!("{server},{port}");
    }
    fields.server = Some(percent_decode(&server));
    for parameter in query.split('&').filter(|parameter| !parameter.is_empty()) {
        let (key, value) = parameter.split_once('=').unwrap_or((parameter, ""));
        fields.set(&percent_decode(key), percent_decode(value));
    }
    fields
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let hex = bytes
            .get(index + 1..index + 3)
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .and_then(|digits| u8::from_str_radix(digits, 16).ok());
        match (byte, hex) {
            (b'%', Some(value)) => {
                decoded.push(value);
                index += 3;
            }
            (b'+', _) => {
                decoded.push(b' ');
                index += 1;
            }
            _ => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Keys ignore case and spaces: `Initial Catalog` and `initialcatalog` are the same.
fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

pub(crate) struct Server {
    pub(crate) host: String,
    pub(crate) port: Option<u16>,
    pub(crate) instance: Option<String>,
}

/// `tcp:host,port`, `host\instance`, `host\instance,port`, `host,port`, or `host`.
/// `.` and `(local)` mean this machine.
pub(crate) fn parse_server(server: &str) -> Result<Server, ConnectionStringError> {
    let server = server.trim();
    let server = server
        .get(..4)
        .filter(|prefix| prefix.eq_ignore_ascii_case("tcp:"))
        .map_or(server, |_| &server[4..]);
    let (name, port) = match server.split_once(',') {
        Some((name, port)) => {
            let port = port
                .trim()
                .parse()
                .map_err(|_| ConnectionStringError::InvalidPort(port.trim().to_owned()))?;
            (name.trim(), Some(port))
        }
        None => (server, None),
    };
    let (host, instance) = match name.split_once('\\') {
        Some((host, instance)) => (host.trim(), Some(instance.trim().to_owned())),
        None => (name, None),
    };
    let host = match host.to_ascii_lowercase().as_str() {
        "" => return Err(ConnectionStringError::MissingServer),
        "." | "(local)" => "localhost".to_owned(),
        _ => host.to_owned(),
    };
    Ok(Server {
        host,
        port,
        instance: instance.filter(|instance| !instance.is_empty()),
    })
}

/// Splits `key=value;` pairs. Values may be quoted with `"`, `'` or `{}`;
/// a doubled closing quote inside is an escaped quote.
fn pairs(text: &str) -> Result<Vec<(String, String)>, ConnectionStringError> {
    let mut pairs = Vec::new();
    let mut rest = text;
    loop {
        rest = rest.trim_start_matches([';', ' ', '\t', '\r', '\n']);
        if rest.is_empty() {
            return Ok(pairs);
        }
        let Some((key, after_key)) = rest.split_once('=') else {
            // A trailing fragment with no `=` carries no setting.
            return Ok(pairs);
        };
        let key = key.trim().to_owned();
        let after_key = after_key.trim_start();
        let (value, remaining) = match after_key.chars().next() {
            Some(open @ ('"' | '\'' | '{')) => {
                let close = if open == '{' { '}' } else { open };
                read_quoted(&after_key[1..], close)
                    .ok_or_else(|| ConnectionStringError::UnterminatedQuote(key.clone()))?
            }
            _ => match after_key.split_once(';') {
                Some((value, remaining)) => (value.trim().to_owned(), remaining),
                None => (after_key.trim().to_owned(), ""),
            },
        };
        pairs.push((key, value));
        rest = remaining;
    }
}

fn read_quoted(text: &str, close: char) -> Option<(String, &str)> {
    let mut value = String::new();
    let mut characters = text.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        if character != close {
            value.push(character);
            continue;
        }
        if characters.peek().is_some_and(|&(_, next)| next == close) {
            value.push(close);
            characters.next();
            continue;
        }
        let remaining = &text[index + close.len_utf8()..];
        return Some((value, remaining));
    }
    None
}

/// `snowflake://` URLs, or .NET-connector strings with an `account=` key.
pub(crate) fn is_snowflake_string(text: &str) -> bool {
    let lower = text.trim().to_ascii_lowercase();
    lower.starts_with("snowflake://")
        || lower.split(';').any(|pair| {
            pair.split_once('=')
                .is_some_and(|(key, _)| normalize_key(key) == "account")
        })
}

/// `snowflake://user:password@account/database/schema?warehouse=..&role=..`
/// (the Go driver's form), or `account=..;user=..;password=..;db=..;schema=..;
/// warehouse=..;role=..;private_key_file=..;private_key_pwd=..` (the .NET form).
pub(crate) fn parse_snowflake_string(
    text: &str,
) -> Result<crate::ConnectionSettings, ConnectionStringError> {
    let text = text.trim();
    let mut values: Vec<(String, String)> = Vec::new();
    if let Some(rest) = text
        .get(12..)
        .filter(|_| text.to_ascii_lowercase().starts_with("snowflake://"))
    {
        let (before_query, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (user_info, location) = match before_query.rsplit_once('@') {
            Some((user_info, location)) => (Some(user_info), location),
            None => (None, before_query),
        };
        if let Some(user_info) = user_info {
            let (user, password) = user_info.split_once(':').unwrap_or((user_info, ""));
            values.push(("user".to_owned(), percent_decode(user)));
            values.push(("password".to_owned(), percent_decode(password)));
        }
        let mut parts = location.split('/');
        values.push((
            "account".to_owned(),
            parts.next().unwrap_or_default().to_owned(),
        ));
        if let Some(database) = parts.next().filter(|part| !part.is_empty()) {
            values.push(("db".to_owned(), percent_decode(database)));
        }
        if let Some(schema) = parts.next().filter(|part| !part.is_empty()) {
            values.push(("schema".to_owned(), percent_decode(schema)));
        }
        for parameter in query.split('&').filter(|parameter| !parameter.is_empty()) {
            let (key, value) = parameter.split_once('=').unwrap_or((parameter, ""));
            values.push((percent_decode(key), percent_decode(value)));
        }
    } else {
        values = pairs(text)?;
    }
    let mut settings = crate::ConnectionSettings::new(crate::DriverKind::Snowflake);
    let (mut user, mut password, mut key_file, mut key_password) =
        (String::new(), String::new(), String::new(), String::new());
    for (key, value) in values {
        let value = value.trim().to_owned();
        let optional = (!value.is_empty()).then(|| value.clone());
        match normalize_key(&key).as_str() {
            "account" => settings.server = value,
            "user" | "username" => user = value,
            "password" => password = value,
            "db" | "database" => settings.database = value,
            "schema" => settings.schema = optional,
            "warehouse" => settings.warehouse = optional,
            "role" => settings.role = optional,
            "private_key_file" | "privatekeyfile" | "private_key_path" => key_file = value,
            "private_key_pwd" | "privatekeypwd" | "private_key_file_pwd" => key_password = value,
            "authenticator"
                if !matches!(
                    normalize_key(&value).as_str(),
                    "snowflake" | "snowflake_jwt"
                ) =>
            {
                return Err(ConnectionStringError::UnsupportedAuthentication(value));
            }
            _ => {}
        }
    }
    if settings.server.is_empty() {
        return Err(ConnectionStringError::MissingServer);
    }
    settings.authentication = if key_file.is_empty() {
        Authentication::Password { user, password }
    } else {
        Authentication::KeyPair {
            user,
            private_key_path: key_file,
            passphrase: key_password,
        }
    };
    Ok(settings)
}

/// The file path from `sqlite://path`, `sqlite:path` or `file:path`.
pub(crate) fn sqlite_path(text: &str) -> Option<String> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    let rest = ["sqlite://", "sqlite:", "file:"]
        .iter()
        .find(|prefix| lower.starts_with(*prefix))
        .and_then(|prefix| text.get(prefix.len()..))?;
    // Options such as `?mode=ro` are not supported; better to refuse than ignore them.
    if rest.contains('?') {
        return None;
    }
    (!rest.is_empty()).then(|| rest.to_owned())
}

/// `postgres://` and `postgresql://` URLs, or libpq `key=value` strings separated by spaces.
pub(crate) fn is_postgres_string(text: &str) -> bool {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    // ADO.NET strings name the server `Server`/`Data Source`; libpq uses `host`.
    let libpq_key = |pair: &str| pair.starts_with("dbname=") || pair.starts_with("host=");
    let ado_key = |pair: &str| {
        let key = pair.split('=').next().unwrap_or_default();
        matches!(
            key.trim(),
            "server" | "data source" | "initial catalog" | "database"
        )
    };
    lower.starts_with("postgres://")
        || lower.starts_with("postgresql://")
        || (lower.split_whitespace().any(libpq_key) && !lower.split(';').any(ado_key))
}

pub(crate) fn parse_postgres_string(
    text: &str,
) -> Result<crate::ConnectionSettings, ConnectionStringError> {
    use tokio_postgres::config::{Host, SslMode};

    let config: tokio_postgres::Config =
        text.trim()
            .parse()
            .map_err(
                |error: tokio_postgres::Error| ConnectionStringError::InvalidValue {
                    key: "PostgreSQL connection string",
                    value: error.to_string(),
                },
            )?;
    let host = match config.get_hosts().first() {
        Some(Host::Tcp(host)) => host.clone(),
        #[cfg(unix)]
        Some(Host::Unix(_)) => {
            return Err(ConnectionStringError::InvalidValue {
                key: "host",
                value: "Unix socket paths are not supported; use a host name".to_owned(),
            });
        }
        None => return Err(ConnectionStringError::MissingServer),
    };
    // `disable` has no equivalent here; `prefer` still connects without TLS.
    let encryption = if config.get_ssl_mode() == SslMode::Require {
        Encryption::Mandatory
    } else {
        Encryption::Optional
    };
    Ok(crate::ConnectionSettings {
        server: host,
        port: config.get_ports().first().copied(),
        database: config.get_dbname().unwrap_or_default().to_owned(),
        authentication: Authentication::Password {
            user: config.get_user().unwrap_or_default().to_owned(),
            password: config
                .get_password()
                .map(|password| String::from_utf8_lossy(password).into_owned())
                .unwrap_or_default(),
        },
        encryption,
        ..crate::ConnectionSettings::new(crate::DriverKind::Postgres)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_azure_data_studio_style_strings() {
        let config = parse_connection_string(
            "Server=tcp:abc.datawarehouse.fabric.microsoft.com,1433;Initial Catalog=analytics;\
             Encrypt=True;TrustServerCertificate=False;Connection Timeout=30;\
             Authentication=Active Directory Interactive;",
        )
        .unwrap();
        assert_eq!(config.host, "abc.datawarehouse.fabric.microsoft.com");
        assert_eq!(config.port, Some(1433));
        assert_eq!(config.database, "analytics");
    }

    #[test]
    fn accepts_synonyms_case_and_quotes() {
        let config = parse_connection_string(
            "data source = host.example,14330 ; DATABASE='my;db' ; authentication = \"ActiveDirectoryDefault\"",
        )
        .unwrap();
        assert_eq!(config.host, "host.example");
        assert_eq!(config.port, Some(14330));
        assert_eq!(config.database, "my;db");
        let config = parse_connection_string("Server=h;Database={a}}b}").unwrap();
        assert_eq!(config.database, "a}b");
    }

    #[test]
    fn bare_server_name_is_allowed() {
        let config = parse_connection_string("abc.datawarehouse.fabric.microsoft.com").unwrap();
        assert_eq!(config.host, "abc.datawarehouse.fabric.microsoft.com");
        assert_eq!(config.port, None);
        assert_eq!(config.database, "");
    }

    #[test]
    fn parses_url_form_with_service_principal() {
        let config = parse_connection_string(
            "sqlserver://client-id%40x:s%3Acret@abc.datawarehouse.fabric.microsoft.com:1433\
             ?database=analytics&fedauth=ActiveDirectoryServicePrincipal&tenant%20id=tenant-1",
        )
        .unwrap();
        assert_eq!(config.host, "abc.datawarehouse.fabric.microsoft.com");
        assert_eq!(config.port, Some(1433));
        assert_eq!(config.database, "analytics");
        assert_eq!(
            config.authentication,
            Authentication::ServicePrincipal {
                tenant_id: "tenant-1".to_owned(),
                client_id: "client-id@x".to_owned(),
                client_secret: "s:cret".to_owned(),
            }
        );
    }

    #[test]
    fn service_principal_tenant_can_follow_the_client_id() {
        let config = parse_connection_string(
            "Server=h;Authentication=Active Directory Service Principal;User ID=app@tenant;Password=pw",
        )
        .unwrap();
        assert_eq!(
            config.authentication,
            Authentication::ServicePrincipal {
                tenant_id: "tenant".to_owned(),
                client_id: "app".to_owned(),
                client_secret: "pw".to_owned(),
            }
        );
        assert!(matches!(
            parse_connection_string("sqlserver://app:pw@h?fedauth=ActiveDirectoryServicePrincipal"),
            Err(ConnectionStringError::ServicePrincipalField(_))
        ));
        assert!(matches!(
            parse_connection_string(
                "sqlserver://app%40t@h?fedauth=ActiveDirectoryServicePrincipal"
            ),
            Err(ConnectionStringError::ServicePrincipalField(_))
        ));
    }

    #[test]
    fn url_form_without_credentials_uses_az_login() {
        let config = parse_connection_string(
            "SQLSERVER://h.example?database=d&fedauth=ActiveDirectoryDefault",
        )
        .unwrap();
        assert_eq!((config.host.as_str(), config.port), ("h.example", None));
        assert_eq!(config.authentication, Authentication::AzureCli);
    }

    #[test]
    fn service_principal_secret_is_not_printed() {
        let auth = Authentication::ServicePrincipal {
            tenant_id: "t".to_owned(),
            client_id: "c".to_owned(),
            client_secret: "hunter2".to_owned(),
        };
        assert!(!format!("{auth:?}").contains("hunter2"));
    }

    #[test]
    fn parses_sql_logins() {
        assert_eq!(
            parse_connection_string("Server=h;Database=d;User ID=sa;Password='p;w'")
                .unwrap()
                .authentication,
            Authentication::Password {
                user: "sa".to_owned(),
                password: "p;w".to_owned(),
            }
        );
        assert_eq!(
            parse_connection_string("sqlserver://sa:p%40ss@h:1433?database=d")
                .unwrap()
                .authentication,
            Authentication::Password {
                user: "sa".to_owned(),
                password: "p@ss".to_owned(),
            }
        );
        assert_eq!(
            parse_connection_string("Server=h;Authentication=SqlPassword"),
            Err(ConnectionStringError::MissingUser)
        );
        let auth = Authentication::Password {
            user: "sa".to_owned(),
            password: "hunter2".to_owned(),
        };
        assert!(!format!("{auth:?}").contains("hunter2"));
    }

    #[test]
    fn reads_windows_and_interactive_logins() {
        for text in [
            "Server=h;Integrated Security=SSPI",
            "Server=h;Trusted_Connection=yes",
        ] {
            assert_eq!(
                parse_connection_string(text).unwrap().authentication,
                Authentication::Integrated
            );
        }
        assert_eq!(
            parse_connection_string(
                "Server=h;Authentication=ActiveDirectoryDeviceCodeFlow;User ID=ana@example.com"
            )
            .unwrap()
            .authentication,
            Authentication::EntraInteractive {
                tenant_id: String::new(),
                user: "ana@example.com".to_owned(),
                client_id: String::new(),
                device_code: true,
            }
        );
    }

    #[test]
    fn rejects_unsupported_logins() {
        assert_eq!(
            parse_connection_string("Server=h;Authentication=ActiveDirectoryPassword"),
            Err(ConnectionStringError::UnsupportedAuthentication(
                "ActiveDirectoryPassword".to_owned()
            ))
        );
    }

    #[test]
    fn parses_instances_and_local_names() {
        let config = parse_connection_string(r"Server=.\SQLEXPRESS;Database=d").unwrap();
        assert_eq!(config.host, "localhost");
        assert_eq!(config.instance.as_deref(), Some("SQLEXPRESS"));
        assert_eq!(config.port, None);
        assert_eq!(config.server_label(), r"localhost\SQLEXPRESS");
        let config = parse_connection_string(r"Server=tcp:db1\SALES,50123").unwrap();
        assert_eq!(config.instance.as_deref(), Some("SALES"));
        assert_eq!(config.port, Some(50123));
        let config = parse_connection_string("sqlserver://sa:x@db1/SALES?database=d").unwrap();
        assert_eq!(config.host, "db1");
        assert_eq!(config.instance.as_deref(), Some("SALES"));
    }

    #[test]
    fn parses_encryption_options() {
        let config = parse_connection_string("Server=h").unwrap();
        assert_eq!(config.encryption, Encryption::Mandatory);
        assert!(!config.trust_server_certificate);
        let config = parse_connection_string(
            "Server=h;Encrypt=Optional;TrustServerCertificate=yes;HostNameInCertificate=db.local",
        )
        .unwrap();
        assert_eq!(config.encryption, Encryption::Optional);
        assert!(config.trust_server_certificate);
        assert_eq!(config.host_name_in_certificate.as_deref(), Some("db.local"));
        assert_eq!(
            parse_connection_string("Server=h;Encrypt=strict")
                .unwrap()
                .encryption,
            Encryption::Strict
        );
        assert!(matches!(
            parse_connection_string("Server=h;Encrypt=maybe"),
            Err(ConnectionStringError::InvalidValue { key: "Encrypt", .. })
        ));
    }

    #[test]
    fn reports_malformed_strings() {
        assert_eq!(
            parse_connection_string("Database=d"),
            Err(ConnectionStringError::MissingServer)
        );
        assert_eq!(
            parse_connection_string("Server=h,abc"),
            Err(ConnectionStringError::InvalidPort("abc".to_owned()))
        );
        assert_eq!(
            parse_connection_string("Server=h;Database='open"),
            Err(ConnectionStringError::UnterminatedQuote(
                "Database".to_owned()
            ))
        );
    }
}
