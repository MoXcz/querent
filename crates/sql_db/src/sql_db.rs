//! Database drivers behind one result shape, so the UI never sees driver types.

mod catalog;
mod connection_settings;
mod connection_string;
mod csv_export;
mod tds;

use std::fmt;
use std::sync::Arc;

pub use catalog::{
    CatalogObject, ColumnInfo, ObjectKind, create_table_script, quote_identifier, select_script,
};
pub use connection_settings::{AuthMethod, ConnectionSettings, DriverKind};
pub use connection_string::{
    ConnectionStringError, parse_connection_string, parse_connection_string_with_database,
};
pub use csv_export::write_csv;
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
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryOutput {
    pub result_sets: Vec<ResultSet>,
    pub rows_affected: Option<u64>,
    pub messages: Vec<String>,
}

/// Cancels a running query on the server. Cheap to clone and safe to call
/// from any thread.
#[derive(Debug, Clone, Default)]
pub struct CancelHandle {
    inner: Arc<mssql_tds::core::CancelHandle>,
}

impl CancelHandle {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.inner.cancel();
    }
}
