//! A saved connection as separate fields, filled in a form or from a connection string.
//!
//! The shape is shared by every driver: each driver lists the sign-in methods it
//! accepts and turns the fields into its own config.

use serde::{Deserialize, Serialize};

use crate::catalog::Dialect;
use crate::connection_string::parse_server;
use crate::postgres::PgConfig;
use crate::snowflake::SnowflakeConfig;
use crate::sqlite::SqliteConfig;
use crate::{Authentication, ConnectionConfig, ConnectionStringError, Encryption, TdsConfig};

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum DriverKind {
    #[default]
    SqlServer,
    Fabric,
    Postgres,
    Sqlite,
    Snowflake,
}

impl DriverKind {
    pub const ALL: [Self; 5] = [
        Self::SqlServer,
        Self::Fabric,
        Self::Postgres,
        Self::Sqlite,
        Self::Snowflake,
    ];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::SqlServer => "SQL Server",
            Self::Fabric => "Microsoft Fabric",
            Self::Postgres => "PostgreSQL",
            Self::Sqlite => "SQLite",
            Self::Snowflake => "Snowflake",
        }
    }

    #[must_use]
    pub fn dialect(self) -> Dialect {
        match self {
            Self::SqlServer | Self::Fabric => Dialect::TSql,
            Self::Postgres => Dialect::Postgres,
            Self::Sqlite => Dialect::Sqlite,
            Self::Snowflake => Dialect::Snowflake,
        }
    }

    /// SQLite opens a file: no host, port, database name or sign-in.
    #[must_use]
    pub fn is_file(self) -> bool {
        match self {
            Self::SqlServer | Self::Fabric | Self::Postgres | Self::Snowflake => false,
            Self::Sqlite => true,
        }
    }

    /// Snowflake's session context: warehouse, role and schema.
    #[must_use]
    pub fn has_session_context(self) -> bool {
        match self {
            Self::SqlServer | Self::Fabric | Self::Postgres | Self::Sqlite => false,
            Self::Snowflake => true,
        }
    }

    /// Encryption choices with this driver's names for them; the first is the default.
    #[must_use]
    pub fn encryption_choices(self) -> &'static [(Encryption, &'static str)] {
        match self {
            Self::SqlServer | Self::Fabric => &[
                (Encryption::Mandatory, "Mandatory"),
                (Encryption::Optional, "Optional"),
                (Encryption::Strict, "Strict (TDS 8.0)"),
            ],
            Self::Postgres => &[
                (Encryption::Mandatory, "Require"),
                (
                    Encryption::Optional,
                    "Prefer (no encryption if the server has none)",
                ),
            ],
            // Snowflake is always HTTPS.
            Self::Sqlite | Self::Snowflake => &[],
        }
    }

    /// The first method is the default for a new connection.
    #[must_use]
    pub fn auth_methods(self) -> &'static [AuthMethod] {
        match self {
            Self::SqlServer => &[
                AuthMethod::Password,
                AuthMethod::Integrated,
                AuthMethod::AzureCli,
                AuthMethod::EntraInteractive,
                AuthMethod::ServicePrincipal,
            ],
            Self::Fabric => &[
                AuthMethod::AzureCli,
                AuthMethod::EntraInteractive,
                AuthMethod::ServicePrincipal,
            ],
            Self::Postgres => &[AuthMethod::Password],
            Self::Sqlite => &[AuthMethod::None],
            Self::Snowflake => &[AuthMethod::Password, AuthMethod::KeyPair],
        }
    }

    /// Whether the server takes a port, and the encryption and certificate options.
    #[must_use]
    pub fn has_network_options(self) -> bool {
        match self {
            Self::SqlServer | Self::Postgres => true,
            // Fabric endpoints are always port 1433 with strict encryption.
            Self::Fabric | Self::Sqlite | Self::Snowflake => false,
        }
    }
}

/// [`Authentication`] without its values, for choosing a method in a form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    Password,
    Integrated,
    AzureCli,
    EntraInteractive,
    ServicePrincipal,
    None,
    KeyPair,
}

impl AuthMethod {
    #[must_use]
    pub fn label(self, driver: DriverKind) -> &'static str {
        match (self, driver) {
            (Self::Password, DriverKind::SqlServer) => "SQL Server login",
            (
                Self::Password,
                DriverKind::Fabric
                | DriverKind::Postgres
                | DriverKind::Sqlite
                | DriverKind::Snowflake,
            ) => "User name and password",
            (Self::KeyPair, _) => "Key pair (private key file)",
            (Self::AzureCli, _) => "Entra ID (az login session)",
            (Self::EntraInteractive, _) => "Entra ID (browser sign-in)",
            (Self::Integrated, _) => "Windows authentication (Kerberos)",
            (Self::ServicePrincipal, _) => "Entra ID service principal",
            (Self::None, _) => "None",
        }
    }

    /// The method with empty values, for a new connection.
    #[must_use]
    pub fn empty(self) -> Authentication {
        match self {
            Self::Password => Authentication::Password {
                user: String::new(),
                password: String::new(),
            },
            Self::AzureCli => Authentication::AzureCli,
            Self::EntraInteractive => Authentication::EntraInteractive {
                tenant_id: String::new(),
                user: String::new(),
                client_id: String::new(),
                device_code: false,
            },
            Self::Integrated => Authentication::Integrated,
            Self::ServicePrincipal => Authentication::ServicePrincipal {
                tenant_id: String::new(),
                client_id: String::new(),
                client_secret: String::new(),
            },
            Self::None => Authentication::None,
            Self::KeyPair => Authentication::KeyPair {
                user: String::new(),
                private_key_path: String::new(),
                passphrase: String::new(),
            },
        }
    }
}

impl From<&Authentication> for AuthMethod {
    fn from(authentication: &Authentication) -> Self {
        match authentication {
            Authentication::Password { .. } => Self::Password,
            Authentication::AzureCli => Self::AzureCli,
            Authentication::EntraInteractive { .. } => Self::EntraInteractive,
            Authentication::Integrated => Self::Integrated,
            Authentication::ServicePrincipal { .. } => Self::ServicePrincipal,
            Authentication::None => Self::None,
            Authentication::KeyPair { .. } => Self::KeyPair,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionSettings {
    pub driver: DriverKind,
    /// Host, or `host\instance` for a named SQL Server instance.
    pub server: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Empty means the server's default; the tab's database field can fill it.
    #[serde(default)]
    pub database: String,
    pub authentication: Authentication,
    #[serde(default)]
    pub encryption: Encryption,
    #[serde(default)]
    pub trust_server_certificate: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_name_in_certificate: Option<String>,
    /// Snowflake session context; empty uses the user's defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warehouse: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
}

impl ConnectionSettings {
    #[must_use]
    pub fn new(driver: DriverKind) -> Self {
        Self {
            driver,
            server: String::new(),
            port: None,
            database: String::new(),
            authentication: driver
                .auth_methods()
                .first()
                .map_or_else(Authentication::default, |method| method.empty()),
            encryption: driver
                .encryption_choices()
                .first()
                .map_or_else(Encryption::default, |(encryption, _)| *encryption),
            trust_server_certificate: false,
            host_name_in_certificate: None,
            warehouse: None,
            role: None,
            schema: None,
        }
    }

    /// SQL Server and Fabric strings (ADO.NET or `sqlserver://`), or PostgreSQL
    /// strings (`postgres://` URL or libpq `host=... dbname=...`).
    pub fn from_connection_string(text: &str) -> Result<Self, ConnectionStringError> {
        if crate::connection_string::is_snowflake_string(text) {
            return crate::connection_string::parse_snowflake_string(text);
        }
        if let Some(path) = crate::connection_string::sqlite_path(text) {
            return Ok(Self {
                server: path,
                ..Self::new(DriverKind::Sqlite)
            });
        }
        if crate::connection_string::is_postgres_string(text) {
            return crate::connection_string::parse_postgres_string(text);
        }
        crate::parse_connection_string(text).map(Self::from)
    }

    pub fn config(&self) -> Result<ConnectionConfig, ConnectionStringError> {
        match self.driver {
            DriverKind::SqlServer | DriverKind::Fabric => {
                self.tds_config().map(ConnectionConfig::Tds)
            }
            DriverKind::Postgres => self.postgres_config().map(ConnectionConfig::Postgres),
            DriverKind::Sqlite => {
                let path = self.server.trim();
                if path.is_empty() {
                    return Err(ConnectionStringError::MissingServer);
                }
                Ok(ConnectionConfig::Sqlite(SqliteConfig::new(path)))
            }
            DriverKind::Snowflake => {
                let account = snowflake_account(&self.server);
                if account.is_empty() {
                    return Err(ConnectionStringError::MissingServer);
                }
                Ok(ConnectionConfig::Snowflake(SnowflakeConfig {
                    warehouse: self.warehouse.clone(),
                    role: self.role.clone(),
                    database: self.database.clone(),
                    schema: self.schema.clone(),
                    ..SnowflakeConfig::new(account, self.authentication.clone())
                }))
            }
        }
    }

    fn postgres_config(&self) -> Result<PgConfig, ConnectionStringError> {
        let host = self.server.trim();
        if host.is_empty() {
            return Err(ConnectionStringError::MissingServer);
        }
        if !matches!(self.authentication, Authentication::Password { .. }) {
            return Err(ConnectionStringError::UnsupportedAuthentication(
                "Entra ID for PostgreSQL".to_owned(),
            ));
        }
        Ok(PgConfig {
            port: self.port,
            authentication: self.authentication.clone(),
            encryption: self.encryption,
            trust_server_certificate: self.trust_server_certificate,
            ..PgConfig::new(host, self.database.clone())
        })
    }

    fn tds_config(&self) -> Result<TdsConfig, ConnectionStringError> {
        let server = parse_server(&self.server)?;
        let port = match (self.port, server.port) {
            (Some(field), Some(in_server)) if field != in_server => {
                return Err(ConnectionStringError::ConflictingPorts(field, in_server));
            }
            (field, in_server) => field.or(in_server),
        };
        let encryption = match self.driver {
            DriverKind::SqlServer
            | DriverKind::Postgres
            | DriverKind::Sqlite
            | DriverKind::Snowflake => self.encryption,
            // Fabric only speaks TDS 8.0, whatever host name the endpoint uses.
            DriverKind::Fabric => Encryption::Strict,
        };
        let config = TdsConfig::new(server.host, self.database.clone());
        Ok(TdsConfig {
            port,
            instance: server.instance,
            authentication: self.authentication.clone(),
            encryption,
            trust_server_certificate: self.trust_server_certificate,
            host_name_in_certificate: self.host_name_in_certificate.clone(),
            ..config
        })
    }
}

/// The account identifier from a typed value that may be the full host name
/// (`xy12345.snowflakecomputing.com`) or a URL.
fn snowflake_account(server: &str) -> String {
    let server = server.trim();
    let server = server
        .strip_prefix("https://")
        .or_else(|| server.strip_prefix("http://"))
        .unwrap_or(server);
    let host = server.split('/').next().unwrap_or_default();
    host.strip_suffix(".snowflakecomputing.com")
        .unwrap_or(host)
        .to_owned()
}

impl From<TdsConfig> for ConnectionSettings {
    fn from(config: TdsConfig) -> Self {
        let driver = if config.is_fabric() {
            DriverKind::Fabric
        } else {
            DriverKind::SqlServer
        };
        let server = match &config.instance {
            Some(instance) => format!("{}\\{instance}", config.host),
            None => config.host,
        };
        Self {
            driver,
            server,
            port: config.port,
            database: config.database,
            authentication: config.authentication,
            encryption: config.encryption,
            trust_server_certificate: config.trust_server_certificate,
            host_name_in_certificate: config.host_name_in_certificate,
            warehouse: None,
            role: None,
            schema: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_from_a_connection_string_and_back() {
        let settings = ConnectionSettings::from_connection_string(
            r"Server=db1\SALES,50123;Database=shop;User ID=sa;Password=pw;TrustServerCertificate=True",
        )
        .unwrap();
        assert_eq!(settings.driver, DriverKind::SqlServer);
        assert_eq!(settings.server, r"db1\SALES");
        assert_eq!(settings.port, Some(50123));
        let config = settings.tds_config().unwrap();
        assert_eq!(config.host, "db1");
        assert_eq!(config.instance.as_deref(), Some("SALES"));
        assert_eq!(config.port, Some(50123));
        assert!(config.trust_server_certificate);
        assert_eq!(
            ConnectionSettings::from_connection_string("x.datawarehouse.fabric.microsoft.com")
                .unwrap()
                .driver,
            DriverKind::Fabric
        );
    }

    #[test]
    fn round_trips_through_toml_with_tagged_authentication() {
        let settings = ConnectionSettings {
            authentication: Authentication::ServicePrincipal {
                tenant_id: "t".to_owned(),
                client_id: "c".to_owned(),
                client_secret: "s".to_owned(),
            },
            ..ConnectionSettings::new(DriverKind::Fabric)
        };
        let text = toml::to_string(&settings).unwrap();
        assert!(text.contains(r#"method = "service_principal""#), "{text}");
        assert_eq!(
            toml::from_str::<ConnectionSettings>(&text).unwrap(),
            settings
        );
    }

    #[test]
    fn fabric_is_strict_and_ports_must_agree() {
        let fabric = ConnectionSettings {
            server: "endpoint.example".to_owned(),
            encryption: Encryption::Optional,
            ..ConnectionSettings::new(DriverKind::Fabric)
        };
        assert_eq!(fabric.tds_config().unwrap().encryption, Encryption::Strict);
        let conflicting = ConnectionSettings {
            server: "h,1433".to_owned(),
            port: Some(1444),
            ..ConnectionSettings::new(DriverKind::SqlServer)
        };
        assert_eq!(
            conflicting.tds_config(),
            Err(ConnectionStringError::ConflictingPorts(1444, 1433))
        );
    }

    #[test]
    fn parses_postgres_strings() {
        let settings = ConnectionSettings::from_connection_string(
            "postgresql://app:s%40cret@db.example:6543/shop?sslmode=require",
        )
        .unwrap();
        assert_eq!(settings.driver, DriverKind::Postgres);
        assert_eq!(settings.server, "db.example");
        assert_eq!(settings.port, Some(6543));
        assert_eq!(settings.database, "shop");
        assert_eq!(settings.encryption, Encryption::Mandatory);
        assert_eq!(
            settings.authentication,
            Authentication::Password {
                user: "app".to_owned(),
                password: "s@cret".to_owned(),
            }
        );
        let settings =
            ConnectionSettings::from_connection_string("host=localhost user=postgres dbname=x")
                .unwrap();
        assert_eq!(settings.driver, DriverKind::Postgres);
        assert_eq!(settings.encryption, Encryption::Mandatory);
        assert_eq!(
            ConnectionSettings::new(DriverKind::Postgres).encryption,
            Encryption::Mandatory
        );
        for (mode, encryption) in [
            ("prefer", Encryption::Optional),
            ("disable", Encryption::Optional),
            ("verify-full", Encryption::Mandatory),
            ("verify-ca", Encryption::Mandatory),
        ] {
            let text = format!("postgres://u@h/db?sslmode={mode}");
            let settings = ConnectionSettings::from_connection_string(&text).unwrap();
            assert_eq!(settings.encryption, encryption, "{mode}");
        }
        let settings =
            ConnectionSettings::from_connection_string("host=h user=u password=mysslmodepw")
                .unwrap();
        assert_eq!(settings.encryption, Encryption::Mandatory);
        assert!(ConnectionSettings::from_connection_string("host=h sslmode=nonsense").is_err());
        assert!(crate::connection_string::is_postgres_string(
            "host=h password=a;b dbname=x"
        ));
        assert!(!crate::connection_string::is_postgres_string(
            "Server=h;Database=d;Host=ignored"
        ));
        assert!(matches!(
            settings.config().unwrap(),
            ConnectionConfig::Postgres(_)
        ));
    }

    #[test]
    fn reads_sqlite_paths() {
        for text in [
            "sqlite:///tmp/shop.db",
            "sqlite:/tmp/shop.db",
            "file:/tmp/shop.db",
        ] {
            let settings = ConnectionSettings::from_connection_string(text).unwrap();
            assert_eq!(settings.driver, DriverKind::Sqlite, "{text}");
            assert_eq!(settings.server, "/tmp/shop.db", "{text}");
            assert_eq!(settings.authentication, Authentication::None);
        }
    }

    #[test]
    fn reads_snowflake_strings() {
        let settings = ConnectionSettings::from_connection_string(
            "snowflake://me:p%40ss@myorg-acct/SALES/PUBLIC?warehouse=WH&role=ANALYST",
        )
        .unwrap();
        assert_eq!(settings.driver, DriverKind::Snowflake);
        assert_eq!(settings.server, "myorg-acct");
        assert_eq!(settings.database, "SALES");
        assert_eq!(settings.schema.as_deref(), Some("PUBLIC"));
        assert_eq!(settings.warehouse.as_deref(), Some("WH"));
        assert_eq!(settings.role.as_deref(), Some("ANALYST"));
        assert_eq!(
            settings.authentication,
            Authentication::Password {
                user: "me".to_owned(),
                password: "p@ss".to_owned(),
            }
        );
        let settings = ConnectionSettings::from_connection_string(
            "account=xy12345.us-east-1;user=svc;private_key_file=/keys/rsa.p8;db=RAW",
        )
        .unwrap();
        assert_eq!(settings.server, "xy12345.us-east-1");
        assert!(matches!(
            settings.authentication,
            Authentication::KeyPair { ref private_key_path, .. } if private_key_path == "/keys/rsa.p8"
        ));
        assert_eq!(
            snowflake_account("https://xy12345.snowflakecomputing.com/"),
            "xy12345"
        );
    }

    #[test]
    fn every_driver_offers_its_saved_method() {
        for driver in DriverKind::ALL {
            assert_ne!(driver.auth_methods(), &[]);
        }
        assert_eq!(
            AuthMethod::from(&Authentication::AzureCli),
            AuthMethod::AzureCli
        );
    }
}
