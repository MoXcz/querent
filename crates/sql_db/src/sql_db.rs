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
pub use csv_export::{Headers, rows_as_tsv, write_csv};
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
    /// A server error that names a line, 1-based, of the text that was sent.
    #[error("{message}")]
    ServerAt { message: String, line: usize },
    #[error("connection lost: {0}")]
    ConnectionLost(String),
    #[error("query cancelled")]
    Cancelled,
}

impl QueryError {
    /// The line of the sent text the server blames, 1-based.
    #[must_use]
    pub fn line(&self) -> Option<usize> {
        match self {
            Self::ServerAt { line, .. } => Some(*line),
            Self::Auth(_)
            | Self::Connect(_)
            | Self::Server(_)
            | Self::ConnectionLost(_)
            | Self::Cancelled => None,
        }
    }

    /// The same error for text that started `lines` lines into what was sent.
    #[must_use]
    pub(crate) fn after_lines(self, lines: usize) -> Self {
        match self {
            Self::ServerAt { message, line } => Self::ServerAt {
                message,
                line: line + lines,
            },
            other @ (Self::Auth(_)
            | Self::Connect(_)
            | Self::Server(_)
            | Self::ConnectionLost(_)
            | Self::Cancelled) => other,
        }
    }
}

/// How many line breaks come before byte `offset` of `text`.
pub(crate) fn lines_before(text: &str, offset: usize) -> usize {
    text.get(..offset)
        .map_or(0, |before| before.matches('\n').count())
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

    /// The order a grid sorts by: NULL, then numbers by value, then text, then bytes.
    /// A total order, as sorting needs: a number never compares as text.
    #[must_use]
    pub fn compare(&self, other: &Self) -> std::cmp::Ordering {
        self.sort_key().cmp(&other.sort_key())
    }

    fn sort_key(&self) -> SortKey<'_> {
        #[expect(clippy::cast_precision_loss, reason = "the exact integer breaks ties")]
        let integer = |number: i64| SortKey::Number(TotalF64(number as f64), Some(number));
        match self {
            Value::Null => SortKey::Null,
            Value::Bool(flag) => integer(i64::from(*flag)),
            Value::Int(number) => integer(*number),
            Value::Float(number) => SortKey::Number(TotalF64(*number), None),
            Value::Decimal(text) => text.parse::<f64>().map_or(SortKey::Text(text), |number| {
                SortKey::Number(TotalF64(number), None)
            }),
            Value::Text(text) | Value::Date(text) | Value::Time(text) | Value::Timestamp(text) => {
                SortKey::Text(text)
            }
            Value::Bytes(bytes) => SortKey::Bytes(bytes),
        }
    }

    #[must_use]
    pub fn is_numeric(&self) -> bool {
        matches!(self, Value::Int(_) | Value::Float(_) | Value::Decimal(_))
    }
}

/// Variants in sort order; a number sorts by its value, then by its exact
/// integer, so two integers that round to one float still differ.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum SortKey<'a> {
    Null,
    Number(TotalF64, Option<i64>),
    Text(&'a str),
    Bytes(&'a [u8]),
}

/// A float under `total_cmp`, which orders NaN too.
struct TotalF64(f64);

impl PartialEq for TotalF64 {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for TotalF64 {}

impl PartialOrd for TotalF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TotalF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
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
    /// A complete result set, as tests and other crates build one.
    #[must_use]
    pub fn new(columns: Vec<Column>, rows: Vec<Vec<Value>>) -> Self {
        let mut result_set = Self {
            columns,
            ..Self::default()
        };
        for row in rows {
            result_set.push_row(row);
        }
        result_set
    }

    /// Rough memory the rows hold.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }

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
    fn values_order_with_null_first_and_numbers_by_value() {
        use std::cmp::Ordering;
        let mut values = vec![
            Value::Int(10),
            Value::Null,
            Value::Decimal("9.5".to_owned()),
            Value::Float(-1.0),
            Value::Int(2),
        ];
        values.sort_by(Value::compare);
        assert_eq!(
            values,
            vec![
                Value::Null,
                Value::Float(-1.0),
                Value::Int(2),
                Value::Decimal("9.5".to_owned()),
                Value::Int(10),
            ]
        );
        let text = |text: &str| Value::Text(text.to_owned());
        assert_eq!(text("b").compare(&text("a")), Ordering::Greater);
        assert_eq!(
            Value::Date("2024-01-02".to_owned()).compare(&Value::Date("2024-01-10".to_owned())),
            Ordering::Less
        );
        assert_eq!(
            Value::Int(i64::MAX).compare(&Value::Int(i64::MAX - 1)),
            Ordering::Greater
        );
        // A column that mixes numbers and text still sorts: numbers first, by value.
        let mut mixed = vec![text("5"), Value::Int(10), text("10"), Value::Int(9)];
        mixed.sort_by(Value::compare);
        assert_eq!(
            mixed,
            vec![Value::Int(9), Value::Int(10), text("10"), text("5")]
        );
        let big = 1_i64 << 53;
        #[expect(
            clippy::cast_precision_loss,
            reason = "the float that both integers round to"
        )]
        let rounded = Value::Float(big as f64);
        assert_eq!(
            Value::Int(big).compare(&Value::Int(big + 1)),
            Ordering::Less
        );
        assert_eq!(rounded.compare(&Value::Int(big)), Ordering::Less);
        assert_eq!(rounded.compare(&Value::Int(big + 1)), Ordering::Less);
        assert_eq!(
            Value::Float(f64::NAN).compare(&Value::Float(f64::NAN)),
            Ordering::Equal
        );
    }

    #[test]
    fn an_error_line_moves_with_the_text_before_it() {
        let error = QueryError::ServerAt {
            message: "bad".to_owned(),
            line: 2,
        };
        let sql = "select 1\nGO\n\nselect\n  oops";
        let batch_start = sql.find("select\n").unwrap();
        assert_eq!(lines_before(sql, batch_start), 3);
        assert_eq!(
            error.after_lines(lines_before(sql, batch_start)).line(),
            Some(5)
        );
        assert_eq!(QueryError::Cancelled.after_lines(3).line(), None);
    }

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
