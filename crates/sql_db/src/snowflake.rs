//! Snowflake driver on `snowflake-connector-rs`.
//!
//! Snowflake runs one statement per request, so a batch is split with
//! `sql_text::statements` and run in order. Each statement keeps its
//! `QueryCanceller`, which the cancel handle uses to abort it on the server.

use std::time::Duration;

use snowflake_connector_rs::{
    AuthConfig, CellValue, Client, ClientConfig, ErrorKind, KeyPairConfig, QueryCanceller,
    SessionConfig, Statement,
};

use crate::catalog::Dialect;
use crate::{
    Authentication, CancelHandle, CatalogObject, Column, ColumnInfo, ObjectKind, QueryError,
    QueryOutput, ResultSet, Value,
};

const CANCEL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for the statement to stop once the abort was sent.
const CANCEL_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnowflakeConfig {
    /// Account identifier, `orgname-accountname` or a locator such as `xy12345.us-east-1`.
    pub account: String,
    /// Only `Password` and `KeyPair` are accepted.
    pub authentication: Authentication,
    pub warehouse: Option<String>,
    pub role: Option<String>,
    pub database: String,
    pub schema: Option<String>,
    /// Rows kept per result set; the rest are skipped and the set is marked truncated.
    pub max_rows: usize,
}

impl SnowflakeConfig {
    #[must_use]
    pub fn new(account: impl Into<String>, authentication: Authentication) -> Self {
        Self {
            account: account.into(),
            authentication,
            warehouse: None,
            role: None,
            database: String::new(),
            schema: None,
            max_rows: 50_000,
        }
    }
}

pub struct SnowflakeConnection {
    config: SnowflakeConfig,
    session: snowflake_connector_rs::Session,
}

impl std::fmt::Debug for SnowflakeConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SnowflakeConnection")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl SnowflakeConnection {
    #[tracing::instrument(skip_all, fields(account = %config.account, database = %config.database))]
    pub async fn connect(config: SnowflakeConfig) -> Result<Self, QueryError> {
        let (user, auth) = auth_config(&config.authentication).await?;
        let mut session_config = SessionConfig::new();
        if let Some(warehouse) = non_empty(config.warehouse.as_deref()) {
            session_config = session_config.with_warehouse(warehouse);
        }
        if let Some(role) = non_empty(config.role.as_deref()) {
            session_config = session_config.with_role(role);
        }
        if let Some(database) = non_empty(Some(&config.database)) {
            session_config = session_config.with_database(database);
        }
        if let Some(schema) = non_empty(config.schema.as_deref()) {
            session_config = session_config.with_schema(schema);
        }
        let client = Client::new(
            ClientConfig::new(user, &config.account, auth).with_session(session_config),
        )
        .map_err(|error| QueryError::Connect(format!("{}: {error}", config.account)))?;
        let session = client
            .create_session()
            .await
            .map_err(|error| connect_error(&config, &error))?;
        Ok(Self { config, session })
    }

    #[must_use]
    pub fn config(&self) -> &SnowflakeConfig {
        &self.config
    }

    #[tracing::instrument(skip_all)]
    pub async fn execute(
        &mut self,
        sql: &str,
        cancel: &CancelHandle,
    ) -> Result<QueryOutput, QueryError> {
        let mut output = QueryOutput::default();
        for range in sql_text::statements(sql) {
            let Some(statement) = sql.get(range) else {
                continue;
            };
            if cancel.token.is_cancelled() {
                return Err(QueryError::Cancelled);
            }
            let result_set = self.run_statement(statement, cancel).await?;
            output.result_sets.push(result_set);
        }
        Ok(output)
    }

    pub fn close(self) {
        drop(self);
    }

    pub async fn list_databases(&mut self) -> Result<Vec<String>, QueryError> {
        // `show databases` needs no access to the shared SNOWFLAKE database.
        let result = self.query("show databases").await?;
        let Some(name_index) = result
            .columns
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case("name"))
        else {
            return Ok(Vec::new());
        };
        let mut names: Vec<String> = result
            .rows
            .iter()
            .filter_map(|row| row.get(name_index))
            .map(ToString::to_string)
            .collect();
        names.sort();
        Ok(names)
    }

    pub async fn list_objects(&mut self, database: &str) -> Result<Vec<CatalogObject>, QueryError> {
        let catalog = Dialect::Snowflake.quote_identifier(database);
        let result = self
            .query(&format!(
                "select table_schema, table_name, \
                     case when table_type = 'VIEW' or table_type = 'MATERIALIZED VIEW' \
                          then 'V' else 'T' end \
                 from {catalog}.information_schema.tables \
                 where table_schema <> 'INFORMATION_SCHEMA' \
                 union all \
                 select procedure_schema, procedure_name, 'P' \
                 from {catalog}.information_schema.procedures \
                 union all \
                 select function_schema, function_name, 'F' \
                 from {catalog}.information_schema.functions \
                 order by 1, 2, 3"
            ))
            .await?;
        let mut objects: Vec<CatalogObject> = result
            .rows
            .iter()
            .filter_map(|row| {
                let [schema, name, kind] = row.as_slice() else {
                    return None;
                };
                let kind = match kind.to_string().as_str() {
                    "T" => ObjectKind::Table,
                    "V" => ObjectKind::View,
                    "P" => ObjectKind::Procedure,
                    "F" => ObjectKind::Function,
                    _ => return None,
                };
                Some(CatalogObject {
                    schema: schema.to_string(),
                    name: name.to_string(),
                    kind,
                })
            })
            .collect();
        // Overloads share a name; sorted by kind too, they are adjacent.
        objects.dedup();
        Ok(objects)
    }

    pub async fn list_columns(
        &mut self,
        database: &str,
        object: &CatalogObject,
    ) -> Result<Vec<ColumnInfo>, QueryError> {
        let catalog = Dialect::Snowflake.quote_identifier(database);
        let result = self
            .query(&format!(
                "select column_name, data_type, is_nullable \
                 from {catalog}.information_schema.columns \
                 where table_schema = {} and table_name = {} order by ordinal_position",
                quote_literal(&object.schema),
                quote_literal(&object.name)
            ))
            .await?;
        Ok(result
            .rows
            .iter()
            .filter_map(|row| {
                let [name, type_name, nullable] = row.as_slice() else {
                    return None;
                };
                Some(ColumnInfo {
                    name: name.to_string(),
                    type_name: type_name.to_string().to_lowercase(),
                    nullable: nullable.to_string() == "YES",
                })
            })
            .collect())
    }

    /// `GET_DDL` for tables and views; the stored body for procedures and functions.
    pub async fn object_definition(
        &mut self,
        database: &str,
        object: &CatalogObject,
    ) -> Result<Option<String>, QueryError> {
        let full_name = format!(
            "{}.{}",
            Dialect::Snowflake.quote_identifier(database),
            object.qualified_name(Dialect::Snowflake)
        );
        let catalog = Dialect::Snowflake.quote_identifier(database);
        let sql = match object.kind {
            ObjectKind::Table => format!("select get_ddl('table', {})", quote_literal(&full_name)),
            ObjectKind::View => format!("select get_ddl('view', {})", quote_literal(&full_name)),
            ObjectKind::Procedure => format!(
                "select '-- procedure ' || procedure_name || argument_signature || '\\n' \
                     || procedure_definition \
                 from {catalog}.information_schema.procedures \
                 where procedure_schema = {} and procedure_name = {}",
                quote_literal(&object.schema),
                quote_literal(&object.name)
            ),
            ObjectKind::Function => format!(
                "select '-- function ' || function_name || argument_signature || '\\n' \
                     || function_definition \
                 from {catalog}.information_schema.functions \
                 where function_schema = {} and function_name = {}",
                quote_literal(&object.schema),
                quote_literal(&object.name)
            ),
        };
        let result = self.query(&sql).await?;
        let bodies = first_column(&result);
        Ok((!bodies.is_empty()).then(|| bodies.join("\n\n")))
    }

    /// Catalog queries are short metadata reads, so like the other drivers'
    /// catalog calls they have no cancel; the explorer drops a stale answer.
    async fn query(&self, sql: &str) -> Result<ResultSet, QueryError> {
        self.run_statement(sql, &CancelHandle::new()).await
    }

    async fn run_statement(
        &self,
        sql: &str,
        cancel: &CancelHandle,
    ) -> Result<ResultSet, QueryError> {
        let handle = self
            .session
            .query_handle(Statement::new(sql.to_owned()))
            .map_err(|error| map_error(&error))?;
        let canceller = handle.canceller();
        let max_rows = self.config.max_rows;
        let work = async move {
            let mut cursor = handle.execute().await?;
            let columns: Vec<Column> = cursor
                .schema()
                .columns()
                .iter()
                .map(|column| Column {
                    name: column.name().to_owned(),
                    type_name: column.ty().as_str().to_owned(),
                })
                .collect();
            let mut result_set = ResultSet {
                columns,
                ..ResultSet::default()
            };
            while let Some(table) = cursor.next_table().await? {
                for row in table.dynamic_rows()? {
                    if result_set.rows.len() >= max_rows {
                        // The rest of the result is never fetched.
                        result_set.truncated = true;
                        return Ok(result_set);
                    }
                    result_set
                        .rows
                        .push(row?.values().iter().map(to_value).collect());
                }
            }
            Ok::<_, snowflake_connector_rs::Error>(result_set)
        };
        tokio::pin!(work);
        let result = tokio::select! {
            result = &mut work => result,
            () = cancel.token.cancelled() => {
                abort(&canceller).await;
                // Never leave the tab running if the abort was lost.
                match tokio::time::timeout(CANCEL_SETTLE_TIMEOUT, work).await {
                    Ok(result) => result,
                    Err(_) => return Err(QueryError::Cancelled),
                }
            }
        };
        result.map_err(|error| map_error(&error))
    }
}

async fn abort(canceller: &QueryCanceller) {
    match tokio::time::timeout(CANCEL_REQUEST_TIMEOUT, canceller.cancel()).await {
        Ok(Ok(status)) => tracing::debug!(?status, "snowflake cancel sent"),
        Ok(Err(error)) => tracing::debug!(%error, "snowflake cancel failed"),
        Err(_) => tracing::debug!("snowflake cancel timed out"),
    }
}

/// The user name and the connector's sign-in for `authentication`.
async fn auth_config(authentication: &Authentication) -> Result<(String, AuthConfig), QueryError> {
    match authentication {
        Authentication::Password { user, password } => {
            Ok((user.clone(), AuthConfig::password(password.clone())))
        }
        Authentication::KeyPair {
            user,
            private_key_path,
            passphrase,
        } => {
            let pem = tokio::fs::read_to_string(private_key_path)
                .await
                .map_err(|error| {
                    QueryError::Auth(format!(
                        "reading the private key {private_key_path}: {error}"
                    ))
                })?;
            let key = if passphrase.is_empty() {
                KeyPairConfig::from_pem(pem)
            } else {
                KeyPairConfig::from_encrypted_pem(pem, passphrase.as_bytes().to_vec())
            };
            Ok((user.clone(), AuthConfig::key_pair(key)))
        }
        Authentication::AzureCli
        | Authentication::ServicePrincipal { .. }
        | Authentication::None => Err(QueryError::Auth(
            "Snowflake connections sign in with a password or a key pair".to_owned(),
        )),
    }
}

fn non_empty(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|text| !text.is_empty())
}

/// `'text'` with `'` and `\` escaped, as Snowflake string literals need both.
fn quote_literal(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn first_column(result: &ResultSet) -> Vec<String> {
    result
        .rows
        .iter()
        .filter_map(|row| row.first())
        .filter(|value| !matches!(value, Value::Null))
        .map(ToString::to_string)
        .collect()
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "CellValue is non_exhaustive; new kinds show as their debug text"
)]
fn to_value(value: &CellValue) -> Value {
    match value {
        CellValue::Null => Value::Null,
        CellValue::Boolean(flag) => Value::Bool(*flag),
        CellValue::Integer(number) => {
            i64::try_from(*number).map_or_else(|_| Value::Decimal(number.to_string()), Value::Int)
        }
        CellValue::Float(number) => Value::Float(*number),
        CellValue::Decimal(decimal) => Value::Decimal(decimal.raw().to_owned()),
        CellValue::String(text) => Value::Text(text.clone()),
        CellValue::Date(date) => Value::Date(date.to_string()),
        CellValue::Time(time) => Value::Time(time.to_string()),
        CellValue::TimestampNtz(timestamp) => Value::Timestamp(timestamp.to_string()),
        CellValue::TimestampLtz(timestamp) => Value::Timestamp(timestamp.to_string()),
        CellValue::TimestampTz(timestamp) => Value::Timestamp(timestamp.to_string()),
        CellValue::Json(json) => Value::Text(json.to_string()),
        CellValue::Binary(bytes) => Value::Bytes(bytes.as_bytes().to_vec()),
        other => Value::Text(format!("{other:?}")),
    }
}

#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "ErrorKind is the connector's; every other kind reads as a server error"
)]
fn map_error(error: &snowflake_connector_rs::Error) -> QueryError {
    match error.kind() {
        ErrorKind::Cancelled => QueryError::Cancelled,
        ErrorKind::Auth => QueryError::Auth(error.to_string()),
        // The tab reconnects after these.
        ErrorKind::SessionExpired | ErrorKind::Network => {
            QueryError::ConnectionLost(error.to_string())
        }
        _ => QueryError::Server(
            error
                .snowflake_message()
                .map_or_else(|| error.to_string(), ToOwned::to_owned),
        ),
    }
}

fn connect_error(config: &SnowflakeConfig, error: &snowflake_connector_rs::Error) -> QueryError {
    let message = format!("{}: {error}", config.account);
    if error.kind() == ErrorKind::Auth {
        QueryError::Auth(message)
    } else {
        QueryError::Connect(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_literals_for_snowflake() {
        assert_eq!(quote_literal(r"it's a\b"), r"'it\'s a\\b'");
    }

    #[test]
    fn maps_cell_values() {
        assert_eq!(to_value(&CellValue::Integer(7)), Value::Int(7));
        assert_eq!(
            to_value(&CellValue::Integer(i128::MAX)),
            Value::Decimal(i128::MAX.to_string())
        );
        assert_eq!(to_value(&CellValue::Null), Value::Null);
        assert_eq!(
            to_value(&CellValue::String("a".to_owned())),
            Value::Text("a".to_owned())
        );
    }

    #[test]
    fn rejects_entra_sign_in() {
        let result = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(auth_config(&Authentication::AzureCli));
        assert!(matches!(result, Err(QueryError::Auth(_))));
    }
}
