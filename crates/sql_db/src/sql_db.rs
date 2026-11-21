//! Database drivers behind one result shape, so the UI never sees driver types.

mod catalog;
mod connection;
mod connection_settings;
mod connection_string;
mod csv_export;
pub mod entra;
mod postgres;
mod snowflake;
mod sqlite;
mod tds;

use std::fmt;
use std::sync::Arc;

pub use catalog::{
    CatalogObject, ColumnInfo, Dialect, ObjectKind, SchemaColumn, create_table_script,
    quote_identifier, select_script,
};
pub use connection::{Connection, ConnectionConfig};
pub use connection_settings::{AuthMethod, ConnectionSettings, DriverKind};
pub use connection_string::{ConnectionStringError, parse_connection_string};
pub use csv_export::write_csv;
pub use postgres::{PgConfig, PgConnection};
pub use snowflake::{SnowflakeConfig, SnowflakeConnection};
pub use sqlite::{SqliteConfig, SqliteConnection};
pub use tds::{Authentication, Encryption, TdsConfig, TdsConnection};

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("sign-in failed: {0}")]
    Auth(String),
    #[error("could not connect: {0}")]
    Connect(String),
    #[error("{0}")]
    Server(String),
    #[error("connection lost: {0}")]
    ConnectionLost(String),
    #[error("query cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub type_name: String,
}

/// Temporal and decimal values are kept as server-formatted text: the grid and
/// CSV export only display them, and text avoids precision loss.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
    Date(String),
    Time(String),
    Timestamp(String),
}

impl Value {
    fn estimated_bytes(&self) -> usize {
        let heap = match self {
            Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_) => 0,
            Value::Decimal(text)
            | Value::Text(text)
            | Value::Date(text)
            | Value::Time(text)
            | Value::Timestamp(text) => text.capacity(),
            Value::Bytes(bytes) => bytes.capacity(),
        };
        std::mem::size_of::<Value>() + heap
    }

    #[must_use]
    pub fn is_numeric(&self) -> bool {
        matches!(self, Value::Int(_) | Value::Float(_) | Value::Decimal(_))
    }
}

impl fmt::Display for Value {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => formatter.write_str("NULL"),
            Value::Bool(value) => write!(formatter, "{}", u8::from(*value)),
            Value::Int(value) => write!(formatter, "{value}"),
            Value::Float(value) => write!(formatter, "{value}"),
            Value::Decimal(text)
            | Value::Text(text)
            | Value::Date(text)
            | Value::Time(text)
            | Value::Timestamp(text) => formatter.write_str(text),
            Value::Bytes(bytes) => {
                formatter.write_str("0x")?;
                bytes
                    .iter()
                    .try_for_each(|byte| write!(formatter, "{byte:02X}"))
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResultSet {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    /// True when the server returned more rows than the fetch limit.
    pub truncated: bool,
    /// Rough memory the rows hold, kept so a run without a row limit cannot exhaust it.
    estimated_bytes: usize,
}

/// Past this estimate a result set is cut like at the row limit.
const MAX_RESULT_BYTES: usize = 1 << 30;

impl ResultSet {
    pub(crate) fn push_row(&mut self, row: Vec<Value>) {
        self.estimated_bytes += std::mem::size_of::<Vec<Value>>()
            + row.iter().map(Value::estimated_bytes).sum::<usize>();
        self.rows.push(row);
    }

    /// Whether the row limit or the memory budget is reached.
    pub(crate) fn is_full(&self, max_rows: usize) -> bool {
        self.rows.len() >= max_rows || self.estimated_bytes >= MAX_RESULT_BYTES
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryOutput {
    /// Shared, so the grid and an export hold a result set without copying its rows.
    pub result_sets: Vec<Arc<ResultSet>>,
    pub rows_affected: Option<u64>,
    pub messages: Vec<String>,
}

/// Cancels a running query on the server. Cheap to clone and safe to call
/// from any thread.
#[must_use = "a handle nobody keeps cannot cancel the query"]
#[derive(Debug, Clone, Default)]
pub struct CancelHandle {
    inner: Arc<mssql_tds::core::CancelHandle>,
    /// Drivers without a cancel hook of their own wait on this and send the cancel request.
    token: tokio_util::sync::CancellationToken,
}

impl CancelHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.inner.cancel();
        self.token.cancel();
    }

    /// Resolves once [`Self::cancel`] is called; for steps with no server-side query, like connecting.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_set_is_full_at_the_row_limit_or_the_memory_budget() {
        let mut result_set = ResultSet::default();
        result_set.push_row(vec![Value::Int(1), Value::Text("abc".to_owned())]);
        assert!(result_set.is_full(1));
        assert!(!result_set.is_full(usize::MAX));
        result_set.push_row(vec![Value::Bytes(Vec::with_capacity(MAX_RESULT_BYTES))]);
        assert!(result_set.is_full(usize::MAX));
    }
}
