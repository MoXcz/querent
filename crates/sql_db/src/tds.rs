//! TDS driver for Microsoft Fabric (and SQL Server in a later version).

use std::time::Duration;

use azure_core::credentials::TokenCredential;
use azure_identity::DeveloperToolsCredential;
use mssql_tds::connection::client_context::{ClientContext, TdsAuthenticationMethod};
use mssql_tds::connection::tds_client::{
    ExecuteOptions, ResultSet as _, StatementResult, TdsClient,
};
use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
use mssql_tds::core::{EncryptionSetting, TdsResult};
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::error::Error as TdsError;

use crate::{CancelHandle, Column, QueryError, QueryOutput, ResultSet, Value};

const SQL_SCOPE: &str = "https://database.windows.net/.default";
const APPLICATION_NAME: &str = "sql-editor";
/// Fabric reports these when it moves or drops a session; one reconnect usually fixes them.
const TRANSIENT_ERRORS: [u32; 3] = [24804, 6005, 6008];
const CONNECT_ATTEMPTS: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricConfig {
    /// SQL endpoint host, e.g. `xxxx.datawarehouse.fabric.microsoft.com`.
    pub host: String,
    /// Warehouse or lakehouse name.
    pub database: String,
    /// Rows kept per result set; the rest are skipped and the set is marked truncated.
    pub max_rows: usize,
}

impl FabricConfig {
    #[must_use]
    pub fn new(host: impl Into<String>, database: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            database: database.into(),
            max_rows: 50_000,
        }
    }
}

pub struct TdsConnection {
    client: TdsClient,
    config: FabricConfig,
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
    /// Signs in with the Azure CLI / Azure Developer CLI session (`az login`).
    #[tracing::instrument(skip_all, fields(host = %config.host, database = %config.database))]
    pub async fn connect_fabric(config: FabricConfig) -> Result<Self, QueryError> {
        let token = fetch_token().await?;
        let mut last_error = None;
        for attempt in 1..=CONNECT_ATTEMPTS {
            match open_client(&config, &token).await {
                Ok(client) => return Ok(Self { client, config }),
                Err(error) if is_transient(&error) && attempt < CONNECT_ATTEMPTS => {
                    tracing::warn!(attempt, "transient connect error, retrying");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    last_error = Some(error);
                }
                Err(error) => return Err(QueryError::Connect(error.to_string())),
            }
        }
        Err(QueryError::Connect(last_error.map_or_else(
            || "no attempts made".to_owned(),
            |error| error.to_string(),
        )))
    }

    #[must_use]
    pub fn config(&self) -> &FabricConfig {
        &self.config
    }

    /// Runs a batch and collects every result set in it.
    #[tracing::instrument(skip_all)]
    pub async fn execute(
        &mut self,
        sql: &str,
        cancel: &CancelHandle,
    ) -> Result<QueryOutput, QueryError> {
        let options = ExecuteOptions::new().cancel(&cancel.inner);
        let result = collect_batch(&mut self.client, sql, options, self.config.max_rows).await;
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

async fn fetch_token() -> Result<String, QueryError> {
    let credential =
        DeveloperToolsCredential::new(None).map_err(|error| QueryError::Auth(error.to_string()))?;
    let token = credential
        .get_token(&[SQL_SCOPE], None)
        .await
        .map_err(|error| QueryError::Auth(format!("{error}. Run `az login` and try again.")))?;
    Ok(token.token.secret().to_owned())
}

async fn open_client(config: &FabricConfig, token: &str) -> TdsResult<TdsClient> {
    let mut context = ClientContext::default();
    context.database.clone_from(&config.database);
    APPLICATION_NAME.clone_into(&mut context.application_name);
    context.tds_authentication_method = TdsAuthenticationMethod::AccessToken;
    context.access_token = Some(token.to_owned());
    context.encryption_options.mode = EncryptionSetting::Strict;
    let datasource = format!("tcp:{},1433", config.host);
    TdsConnectionProvider {}
        .create_client(context, &datasource, None)
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

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "mssql-tds has dozens of error variants; only these matter here"
)]
fn is_transient(error: &TdsError) -> bool {
    match error {
        TdsError::SqlServerError { diagnostics } => diagnostics
            .errors
            .iter()
            .any(|info| TRANSIENT_ERRORS.contains(&info.number)),
        _ => false,
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
        ColumnValues::String(text) => Value::Text(text.to_string()),
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
