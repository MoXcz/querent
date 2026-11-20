//! A saved connection as separate fields, filled in a form or from a connection string.
//!
//! The shape is shared by every driver: each driver lists the sign-in methods it
//! accepts and turns the fields into its own config.

use serde::{Deserialize, Serialize};

use crate::connection_string::parse_server;
use crate::{Authentication, ConnectionStringError, Encryption, TdsConfig};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverKind {
    #[default]
    SqlServer,
    Fabric,
}

impl DriverKind {
    pub const ALL: [Self; 2] = [Self::SqlServer, Self::Fabric];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::SqlServer => "SQL Server",
            Self::Fabric => "Microsoft Fabric",
        }
    }

    /// The first method is the default for a new connection.
    #[must_use]
    pub fn auth_methods(self) -> &'static [AuthMethod] {
        match self {
            Self::SqlServer => &[
                AuthMethod::Password,
                AuthMethod::AzureCli,
                AuthMethod::ServicePrincipal,
            ],
            Self::Fabric => &[AuthMethod::AzureCli, AuthMethod::ServicePrincipal],
        }
    }

    /// Whether the server takes a port, and the encryption and certificate options.
    #[must_use]
    pub fn has_network_options(self) -> bool {
        match self {
            Self::SqlServer => true,
            // Fabric endpoints are always port 1433 with strict encryption.
            Self::Fabric => false,
        }
    }
}

/// [`Authentication`] without its values, for choosing a method in a form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    Password,
    AzureCli,
    ServicePrincipal,
}

impl AuthMethod {
    #[must_use]
    pub fn label(self, driver: DriverKind) -> &'static str {
        match (self, driver) {
            (Self::Password, DriverKind::SqlServer) => "SQL Server login",
            // Not offered for Fabric; named only for a connection string that asks for it.
            (Self::Password, DriverKind::Fabric) => "User name and password",
            (Self::AzureCli, DriverKind::SqlServer | DriverKind::Fabric) => {
                "Entra ID (az login session)"
            }
            (Self::ServicePrincipal, DriverKind::SqlServer | DriverKind::Fabric) => {
                "Entra ID service principal"
            }
        }
    }
}

impl From<&Authentication> for AuthMethod {
    fn from(authentication: &Authentication) -> Self {
        match authentication {
            Authentication::Password { .. } => Self::Password,
            Authentication::AzureCli => Self::AzureCli,
            Authentication::ServicePrincipal { .. } => Self::ServicePrincipal,
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
}

impl ConnectionSettings {
    #[must_use]
    pub fn new(driver: DriverKind) -> Self {
        Self {
            driver,
            server: String::new(),
            port: None,
            database: String::new(),
            authentication: Authentication::default(),
            encryption: Encryption::default(),
            trust_server_certificate: false,
            host_name_in_certificate: None,
        }
    }

    pub fn from_connection_string(text: &str) -> Result<Self, ConnectionStringError> {
        crate::parse_connection_string(text).map(Self::from)
    }

    pub fn tds_config(&self) -> Result<TdsConfig, ConnectionStringError> {
        let server = parse_server(&self.server)?;
        let port = match (self.port, server.port) {
            (Some(field), Some(in_server)) if field != in_server => {
                return Err(ConnectionStringError::ConflictingPorts(field, in_server));
            }
            (field, in_server) => field.or(in_server),
        };
        let encryption = match self.driver {
            DriverKind::SqlServer => self.encryption,
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
