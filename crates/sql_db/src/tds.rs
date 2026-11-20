//! TDS driver for SQL Server and Microsoft Fabric.

use std::sync::Arc;
use std::time::Duration;

use azure_core::credentials::TokenCredential;
use azure_identity::{ClientSecretCredential, DeveloperToolsCredential};
use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use mssql_tds::connection::tds_client::{
    ExecuteOptions, ResultSet as _, StatementResult, TdsClient,
};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{EncryptionSetting, TdsResult};
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::sql_string::SqlString;
use mssql_tds::error::Error as TdsError;

use crate::{CancelHandle, Column, QueryError, QueryOutput, ResultSet, Value};

const SQL_SCOPE: &str = "https://database.windows.net/.default";
const APPLICATION_NAME: &str = "sql-editor";
pub(crate) const DEFAULT_PORT: u16 = 1433;
/// Fabric reports these when it moves or drops a session; one reconnect usually fixes them.
const TRANSIENT_ERRORS: [u32; 3] = [24804, 6005, 6008];
const CONNECT_ATTEMPTS: u32 = 2;

/// How the connection signs in. Saved in `connections.toml`, tagged by `method`.
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Authentication {
    /// Entra ID token from the signed-in Azure CLI (or Azure Developer CLI) session.
    #[default]
    AzureCli,
    /// Entra ID sign-in in the browser, or with a device code; the token is cached and refreshed.
    EntraInteractive {
        /// Empty means any work or school account.
        #[serde(default)]
        tenant_id: String,
        /// Account to preselect; empty lets the user pick.
        #[serde(default)]
        user: String,
        /// Empty means the Azure SDKs' default public client.
        #[serde(default)]
        client_id: String,
        #[serde(default)]
        device_code: bool,
    },
    /// Windows authentication: SSPI on Windows, a Kerberos ticket (`kinit`) elsewhere.
    Integrated,
    /// Entra ID token from an app registration's client credentials.
    ServicePrincipal {
        tenant_id: String,
        client_id: String,
        client_secret: String,
    },
    /// User name and password: a SQL Server login, a PostgreSQL or Snowflake user.
    Password { user: String, password: String },
    /// No sign-in, for a SQLite file.
    None,
    /// Snowflake key-pair sign-in: the private key file stays where it is.
    KeyPair {
        user: String,
        private_key_path: String,
        /// Empty for an unencrypted key.
        passphrase: String,
    },
}

impl std::fmt::Debug for Authentication {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AzureCli => formatter.write_str("AzureCli"),
            Self::EntraInteractive {
                tenant_id,
                user,
                client_id,
                device_code,
            } => formatter
                .debug_struct("EntraInteractive")
                .field("tenant_id", tenant_id)
                .field("user", user)
                .field("client_id", client_id)
                .field("device_code", device_code)
                .finish(),
            Self::Integrated => formatter.write_str("Integrated"),
            Self::ServicePrincipal {
                tenant_id,
                client_id,
                ..
            } => formatter
                .debug_struct("ServicePrincipal")
                .field("tenant_id", tenant_id)
                .field("client_id", client_id)
                .finish_non_exhaustive(),
            Self::Password { user, .. } => formatter
                .debug_struct("Password")
                .field("user", user)
                .finish_non_exhaustive(),
            Self::None => formatter.write_str("None"),
            Self::KeyPair {
                user,
                private_key_path,
                ..
            } => formatter
                .debug_struct("KeyPair")
                .field("user", user)
                .field("private_key_path", private_key_path)
                .finish_non_exhaustive(),
        }
    }
}

impl Authentication {
    /// The password, client secret or key passphrase, which belongs in the keychain.
    #[must_use]
    pub fn secret(&self) -> Option<&str> {
        match self {
            Self::Password { password, .. } => Some(password),
            Self::ServicePrincipal { client_secret, .. } => Some(client_secret),
            Self::KeyPair { passphrase, .. } => Some(passphrase),
            Self::AzureCli | Self::EntraInteractive { .. } | Self::Integrated | Self::None => None,
        }
    }

    pub fn secret_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::Password { password, .. } => Some(password),
            Self::ServicePrincipal { client_secret, .. } => Some(client_secret),
            Self::KeyPair { passphrase, .. } => Some(passphrase),
            Self::AzureCli | Self::EntraInteractive { .. } | Self::Integrated | Self::None => None,
        }
    }
}

/// `Encrypt=` in a connection string.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encryption {
    /// Encrypt only if the server asks for it (`Encrypt=False` or `Optional`).
    Optional,
    /// TLS after pre-login (`Encrypt=True` or `Mandatory`), the `SqlClient` default.
    #[default]
    Mandatory,
    /// TDS 8.0: TLS from the first byte (`Encrypt=Strict`).
    Strict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TdsConfig {
    /// Server host name or address, e.g. `xxxx.datawarehouse.fabric.microsoft.com`.
    pub host: String,
    /// `None` means 1433, or the port SQL Server Browser reports for `instance`.
    pub port: Option<u16>,
    /// Named instance, as in `host\SQLEXPRESS`.
    pub instance: Option<String>,
    /// Database, warehouse or lakehouse name.
    pub database: String,
    pub authentication: Authentication,
    pub encryption: Encryption,
    /// Skips certificate validation, for servers with self-signed certificates.
    pub trust_server_certificate: bool,
    /// Name the certificate must carry when it differs from `host`.
    pub host_name_in_certificate: Option<String>,
    /// Rows kept per result set; the rest are skipped and the set is marked truncated.
    pub max_rows: usize,
}

impl TdsConfig {
    #[must_use]
    pub fn new(host: impl Into<String>, database: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: None,
            instance: None,
            database: database.into(),
            authentication: Authentication::AzureCli,
            encryption: Encryption::Mandatory,
            trust_server_certificate: false,
            host_name_in_certificate: None,
            max_rows: 50_000,
        }
    }

    /// Fabric SQL endpoints only accept TDS 8.0.
    #[must_use]
    pub fn is_fabric(&self) -> bool {
        self.host
            .to_ascii_lowercase()
            .ends_with(".fabric.microsoft.com")
    }

    /// `host,port`, `host\instance` or `host`, for messages.
    #[must_use]
    pub fn server_label(&self) -> String {
        match (&self.instance, self.port) {
            (_, Some(port)) => format!("{},{port}", self.host),
            (Some(instance), None) => format!("{}\\{instance}", self.host),
            (None, None) => self.host.clone(),
        }
    }

    fn data_source(&self) -> String {
        match (&self.instance, self.port) {
            (Some(instance), None) => format!("{}\\{instance}", self.host),
            (_, port) => format!("tcp:{},{}", self.host, port.unwrap_or(DEFAULT_PORT)),
        }
    }

    fn encryption_setting(&self) -> EncryptionSetting {
        if self.is_fabric() {
            return EncryptionSetting::Strict;
        }
        match self.encryption {
            Encryption::Optional => EncryptionSetting::PreferOff,
            Encryption::Mandatory => EncryptionSetting::On,
            Encryption::Strict => EncryptionSetting::Strict,
        }
    }
}

pub struct TdsConnection {
    client: TdsClient,
    config: TdsConfig,
}

impl std::fmt::Debug for TdsConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TdsConnection")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl TdsConnection {
    #[tracing::instrument(skip_all, fields(server = %config.server_label(), database = %config.database))]
    pub async fn connect(config: TdsConfig) -> Result<Self, QueryError> {
        let login = login(&config.authentication).await?;
        let mut last_error = None;
        for attempt in 1..=CONNECT_ATTEMPTS {
            match open_client(&config, &login).await {
                Ok(client) => return Ok(Self { client, config }),
                Err(error) if is_transient(&error) && attempt < CONNECT_ATTEMPTS => {
                    tracing::warn!(attempt, "transient connect error, retrying");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    last_error = Some(error);
                }
                Err(error) => return Err(connect_error(&config, &error)),
            }
        }
        Err(QueryError::Connect(last_error.map_or_else(
            || "no attempts made".to_owned(),
            |error| error.to_string(),
        )))
    }

    #[must_use]
    pub fn config(&self) -> &TdsConfig {
        &self.config
    }

    /// Changes the row limit for later runs on this open connection.
    pub fn set_max_rows(&mut self, max_rows: usize) {
        self.config.max_rows = max_rows;
    }

    /// Runs a batch and collects every result set in it.
    #[tracing::instrument(skip_all)]
    pub async fn execute(
        &mut self,
        sql: &str,
        cancel: &CancelHandle,
    ) -> Result<QueryOutput, QueryError> {
        let max_rows = self.config.max_rows;
        self.execute_with_limit(sql, cancel, max_rows).await
    }

    /// `execute` with its own row cap; catalog reads pass `usize::MAX`.
    pub(crate) async fn execute_with_limit(
        &mut self,
        sql: &str,
        cancel: &CancelHandle,
        max_rows: usize,
    ) -> Result<QueryOutput, QueryError> {
        let options = ExecuteOptions::new().cancel(&cancel.inner);
        let result = collect_batch(&mut self.client, sql, options, max_rows).await;
        let messages = self
            .client
            .take_info_messages()
            .into_iter()
            .map(|message| message.message)
            .collect::<Vec<_>>();
        match result {
            Ok(mut output) => {
                output.messages = messages;
                Ok(output)
            }
            Err(error) => {
                // Leave the connection ready for the next batch; a close failure here
                // is secondary to the error the user needs to see.
                if let Err(close_error) = self.client.close_query().await {
                    tracing::debug!(%close_error, "close_query after failed batch");
                }
                Err(map_error(error))
            }
        }
    }

    pub async fn close(mut self) -> Result<(), QueryError> {
        self.client
            .close_connection()
            .await
            .map_err(|error| QueryError::ConnectionLost(error.to_string()))
    }
}

/// Error 18456 is a rejected login; anything else is a network or TLS problem.
fn connect_error(config: &TdsConfig, error: &TdsError) -> QueryError {
    let server = config.server_label();
    if error_numbers(error).contains(&LOGIN_FAILED) {
        return QueryError::Auth(format!("{server}: {error}"));
    }
    let text = error.to_string();
    let hint = if text.contains("certificate verify failed") && !config.trust_server_certificate {
        " The server's certificate is not trusted; add TrustServerCertificate=True if you trust this server."
    } else {
        ""
    };
    QueryError::Connect(format!("{server}: {text}{hint}"))
}

const LOGIN_FAILED: u32 = 18456;

/// What goes into the login packet; resolved once, so a retry reuses the token.
enum Login {
    Password { user: String, password: String },
    Token(String),
    Integrated,
}

async fn login(authentication: &Authentication) -> Result<Login, QueryError> {
    let auth_error = |error: azure_core::Error| QueryError::Auth(error.to_string());
    let (credential, hint): (Arc<dyn TokenCredential>, &str) = match authentication {
        Authentication::Password { user, password } => {
            return Ok(Login::Password {
                user: user.clone(),
                password: password.clone(),
            });
        }
        Authentication::Integrated => return Ok(Login::Integrated),
        Authentication::EntraInteractive {
            tenant_id,
            user,
            client_id,
            device_code,
        } => {
            let request = crate::entra::SignInRequest {
                tenant_id: tenant_id.clone(),
                user: user.clone(),
                client_id: client_id.clone(),
                device_code: *device_code,
            };
            return crate::entra::access_token(&request).await.map(Login::Token);
        }
        Authentication::None | Authentication::KeyPair { .. } => {
            return Err(QueryError::Auth(
                "SQL Server and Fabric need a sign-in method".to_owned(),
            ));
        }
        Authentication::AzureCli => (
            DeveloperToolsCredential::new(None).map_err(auth_error)?,
            " Run `az login` and try again.",
        ),
        Authentication::ServicePrincipal {
            tenant_id,
            client_id,
            client_secret,
        } => (
            ClientSecretCredential::new(
                tenant_id,
                client_id.clone(),
                client_secret.clone().into(),
                None,
            )
            .map_err(auth_error)?,
            " Check the service principal's tenant, client ID and secret.",
        ),
    };
    let token = credential
        .get_token(&[SQL_SCOPE], None)
        .await
        .map_err(|error| QueryError::Auth(format!("{error}.{hint}")))?;
    Ok(Login::Token(token.token.secret().to_owned()))
}

async fn open_client(config: &TdsConfig, login: &Login) -> TdsResult<TdsClient> {
    let mut context = ClientContext::default();
    context.database.clone_from(&config.database);
    APPLICATION_NAME.clone_into(&mut context.application_name);
    match login {
        Login::Password { user, password } => {
            context.tds_authentication_method = TdsAuthenticationMethod::Password;
            context.user_name.clone_from(user);
            context.password.clone_from(password);
        }
        Login::Token(token) => {
            context.tds_authentication_method = TdsAuthenticationMethod::AccessToken;
            context.access_token = Some(token.clone());
        }
        Login::Integrated => {
            context.tds_authentication_method = TdsAuthenticationMethod::SSPI;
        }
    }
    context.encryption_options.mode = config.encryption_setting();
    context.encryption_options.trust_server_certificate = config.trust_server_certificate;
    context
        .encryption_options
        .host_name_in_cert
        .clone_from(&config.host_name_in_certificate);
    TdsConnectionProvider {}
        .create_client(context, &config.data_source(), None)
        .await
}

async fn collect_batch(
    client: &mut TdsClient,
    sql: &str,
    options: ExecuteOptions<'_>,
    max_rows: usize,
) -> TdsResult<QueryOutput> {
    let mut output = QueryOutput::default();
    let mut statement = client.execute(sql.to_owned(), options).await?;
    loop {
        match statement {
            StatementResult::Rows => {
                output
                    .result_sets
                    .push(read_result_set(client, max_rows).await?);
            }
            StatementResult::NoRows { rows_affected } => {
                if let Some(count) = rows_affected {
                    *output.rows_affected.get_or_insert(0) += count;
                }
            }
            StatementResult::End => break,
        }
        statement = client.advance().await?;
    }
    client.close_query().await?;
    Ok(output)
}

async fn read_result_set(client: &mut TdsClient, max_rows: usize) -> TdsResult<ResultSet> {
    let columns = client
        .get_metadata()
        .iter()
        .map(|metadata| Column {
            name: metadata.column_name.clone(),
            type_name: format!("{:?}", metadata.data_type),
        })
        .collect();
    let mut result_set = ResultSet {
        columns,
        ..ResultSet::default()
    };
    while let Some(row) = client.next_row().await? {
        if result_set.rows.len() == max_rows {
            // `advance` drains the remaining rows without decoding them into values.
            result_set.truncated = true;
            break;
        }
        result_set
            .rows
            .push(row.into_iter().map(to_value).collect());
    }
    Ok(result_set)
}

fn is_transient(error: &TdsError) -> bool {
    error_numbers(error)
        .iter()
        .any(|number| TRANSIENT_ERRORS.contains(number))
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "mssql-tds has dozens of error variants; only server errors carry numbers"
)]
fn error_numbers(error: &TdsError) -> Vec<u32> {
    match error {
        TdsError::SqlServerError { diagnostics } => {
            diagnostics.errors.iter().map(|info| info.number).collect()
        }
        _ => Vec::new(),
    }
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "mssql-tds has dozens of error variants; only these matter here"
)]
fn map_error(error: TdsError) -> QueryError {
    match error {
        TdsError::OperationCancelledError(_) => QueryError::Cancelled,
        TdsError::SqlServerError { .. } => QueryError::Server(error.to_string()),
        other => QueryError::ConnectionLost(other.to_string()),
    }
}

fn to_value(value: ColumnValues) -> Value {
    match value {
        ColumnValues::Null => Value::Null,
        ColumnValues::Bit(value) => Value::Bool(value),
        ColumnValues::TinyInt(value) => Value::Int(i64::from(value)),
        ColumnValues::SmallInt(value) => Value::Int(i64::from(value)),
        ColumnValues::Int(value) => Value::Int(i64::from(value)),
        ColumnValues::BigInt(value) => Value::Int(value),
        ColumnValues::Real(value) => Value::Float(f64::from(value)),
        ColumnValues::Float(value) => Value::Float(value),
        ColumnValues::Decimal(parts) | ColumnValues::Numeric(parts) => {
            Value::Decimal(parts.to_string())
        }
        ColumnValues::SmallMoney(money) => Value::Decimal(format_money(i64::from(money.int_val))),
        ColumnValues::Money(money) => Value::Decimal(format_money(
            (i64::from(money.msb_part) << 32) | i64::from(money.lsb_part.cast_unsigned()),
        )),
        ColumnValues::String(text) => Value::Text(decode_sql_string(text)),
        ColumnValues::Xml(xml) => Value::Text(xml.as_string()),
        ColumnValues::Json(json) => Value::Text(String::from_utf8_lossy(&json.bytes).into_owned()),
        ColumnValues::Uuid(uuid) => Value::Text(uuid.to_string()),
        ColumnValues::Bytes(bytes) => Value::Bytes(bytes),
        ColumnValues::Date(date) => Value::Date(format_date(i64::from(date.get_days()))),
        ColumnValues::Time(time) => Value::Time(format_time(
            time_to_nanoseconds(time.time_nanoseconds),
            time.scale,
        )),
        ColumnValues::DateTime2(value) => Value::Timestamp(format_timestamp(
            i64::from(value.days),
            time_to_nanoseconds(value.time.time_nanoseconds),
            value.time.scale,
        )),
        ColumnValues::DateTimeOffset(value) => {
            // The wire value is UTC; SQL Server shows it in the stored offset.
            let utc = time_to_nanoseconds(value.datetime2.time.time_nanoseconds);
            let local = i128::from(utc) + i128::from(value.offset) * 60 * NANOSECONDS_PER_SECOND;
            Value::Timestamp(format!(
                "{} {}",
                format_timestamp_i128(
                    i64::from(value.datetime2.days),
                    local,
                    value.datetime2.time.scale
                ),
                format_offset(value.offset)
            ))
        }
        ColumnValues::DateTime(value) => {
            // 300 ticks per second, shown rounded to milliseconds like SQL Server does.
            let milliseconds = (u64::from(value.time) * 10 + 1) / 3;
            Value::Timestamp(format_timestamp(
                DAYS_0001_TO_1900 + i64::from(value.days),
                milliseconds * 1_000_000,
                3,
            ))
        }
        ColumnValues::SmallDateTime(value) => Value::Timestamp(format_timestamp(
            DAYS_0001_TO_1900 + i64::from(value.days),
            u64::from(value.time) * 60 * 1_000_000_000,
            0,
        )),
        ColumnValues::Vector(vector) => Value::Text(format!("{vector:?}")),
    }
}

const NANOSECONDS_PER_SECOND: i128 = 1_000_000_000;
const NANOSECONDS_PER_DAY: i128 = 86_400 * NANOSECONDS_PER_SECOND;

/// `SqlTime::time_nanoseconds` actually holds 100 ns ticks (scale 7 units).
fn time_to_nanoseconds(ticks: u64) -> u64 {
    ticks.saturating_mul(100)
}

fn format_timestamp(days_since_0001: i64, nanoseconds: u64, scale: u8) -> String {
    format_timestamp_i128(days_since_0001, i128::from(nanoseconds), scale)
}

/// Accepts a time outside one day so offsets can roll the date forward or back.
fn format_timestamp_i128(days_since_0001: i64, nanoseconds: i128, scale: u8) -> String {
    let day_shift = nanoseconds.div_euclid(NANOSECONDS_PER_DAY);
    let time_of_day = nanoseconds.rem_euclid(NANOSECONDS_PER_DAY);
    let days = i128::from(days_since_0001) + day_shift;
    format!(
        "{} {}",
        format_date(i64::try_from(days).unwrap_or(days_since_0001)),
        format_time(u64::try_from(time_of_day).unwrap_or(0), scale)
    )
}

/// mssql-tds's own `Display` prints `char`/`varchar` with a legacy collation as a
/// byte list, and `to_utf8_string` panics on invalid UTF-8; decode lossily instead.
fn decode_sql_string(text: SqlString) -> String {
    let (bytes, encoding) = text.into_parts();
    match encoding.encoding() {
        Some(encoding) => encoding.decode_without_bom_handling(&bytes).0.into_owned(),
        None => String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// Days from 0001-01-01 to 1900-01-01 in the proleptic Gregorian calendar.
const DAYS_0001_TO_1900: i64 = 693_595;

/// Formats days since 0001-01-01 as `YYYY-MM-DD`.
fn format_date(days_since_0001: i64) -> String {
    // Howard Hinnant's civil_from_days, shifted from the 1970 epoch to year 1.
    let z = days_since_0001 - 719_162 + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn format_time(nanoseconds: u64, scale: u8) -> String {
    let seconds = nanoseconds / 1_000_000_000;
    let fraction = nanoseconds % 1_000_000_000;
    let clock = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    let scale = usize::from(scale.min(9));
    if scale == 0 {
        return clock;
    }
    let digits = format!("{fraction:09}");
    format!("{clock}.{}", digits.get(..scale).unwrap_or(&digits))
}

fn format_offset(offset_minutes: i16) -> String {
    let sign = if offset_minutes < 0 { '-' } else { '+' };
    let minutes = offset_minutes.unsigned_abs();
    format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

/// Money is an integer count of 1/10000 units.
fn format_money(ten_thousandths: i64) -> String {
    let sign = if ten_thousandths < 0 { "-" } else { "" };
    let magnitude = ten_thousandths.unsigned_abs();
    format!("{sign}{}.{:04}", magnitude / 10_000, magnitude % 10_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_dates_from_year_one() {
        assert_eq!(format_date(0), "0001-01-01");
        assert_eq!(format_date(DAYS_0001_TO_1900), "1900-01-01");
        assert_eq!(format_date(738_885), "2024-01-01");
        assert_eq!(format_date(738_945), "2024-03-01");
    }

    #[test]
    fn formats_time_with_scale() {
        let nanoseconds = (13 * 3600 + 5 * 60 + 9) * 1_000_000_000 + 123_456_700;
        assert_eq!(format_time(nanoseconds, 7), "13:05:09.1234567");
        assert_eq!(format_time(nanoseconds, 3), "13:05:09.123");
        assert_eq!(format_time(nanoseconds, 0), "13:05:09");
    }

    #[test]
    fn rolls_date_when_offset_crosses_midnight() {
        let utc_23h = 23 * 3600 * NANOSECONDS_PER_SECOND;
        let local = utc_23h + 330 * 60 * NANOSECONDS_PER_SECOND;
        assert_eq!(
            format_timestamp_i128(738_885, local, 0),
            "2024-01-02 04:30:00"
        );
        assert_eq!(
            format_timestamp_i128(738_885, -NANOSECONDS_PER_SECOND, 0),
            "2023-12-31 23:59:59"
        );
    }

    #[test]
    fn formats_money_and_offsets() {
        assert_eq!(format_money(12_345_678), "1234.5678");
        assert_eq!(format_money(-5), "-0.0005");
        assert_eq!(format_offset(-330), "-05:30");
        assert_eq!(format_offset(60), "+01:00");
    }
}
