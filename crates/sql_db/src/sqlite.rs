//! SQLite driver on `rusqlite` with the bundled SQLite library.
//!
//! SQLite calls block, so each batch runs on tokio's blocking pool; cancel
//! interrupts it through the connection's `InterruptHandle`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use rusqlite::fallible_iterator::FallibleIterator as _;
use rusqlite::types::ValueRef;
use rusqlite::{Batch, ErrorCode, InterruptHandle, OpenFlags};
use tokio_util::sync::CancellationToken;

use crate::catalog::Dialect;
use crate::{
    CancelHandle, CatalogObject, Column, ColumnInfo, ObjectKind, QueryError, QueryOutput,
    ResultSet, Value,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteConfig {
    pub path: PathBuf,
    /// Rows kept per result set; the rest are skipped and the set is marked truncated.
    pub max_rows: usize,
}

impl SqliteConfig {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            max_rows: 50_000,
        }
    }
}

pub struct SqliteConnection {
    config: SqliteConfig,
    // A std mutex: it is only locked inside blocking tasks, never across an await.
    connection: Arc<Mutex<rusqlite::Connection>>,
    interrupt: Arc<InterruptHandle>,
}

impl std::fmt::Debug for SqliteConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqliteConnection")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl SqliteConnection {
    /// Opens an existing file; a mistyped path is an error, not a new empty database.
    #[tracing::instrument(skip_all)]
    pub async fn connect(config: SqliteConfig) -> Result<Self, QueryError> {
        let path = config.path.clone();
        let connection = blocking(move || {
            rusqlite::Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_URI
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|error| QueryError::Connect(format!("{}: {error}", path.display())))
        })
        .await?;
        let interrupt = Arc::new(connection.get_interrupt_handle());
        Ok(Self {
            config,
            connection: Arc::new(Mutex::new(connection)),
            interrupt,
        })
    }

    #[must_use]
    pub fn config(&self) -> &SqliteConfig {
        &self.config
    }

    /// Changes the row limit for later runs on this open connection.
    pub fn set_max_rows(&mut self, max_rows: usize) {
        self.config.max_rows = max_rows;
    }

    #[tracing::instrument(skip_all)]
    pub async fn execute_into(
        &mut self,
        sql: &str,
        cancel: &CancelHandle,
        output: &mut QueryOutput,
    ) -> Result<(), QueryError> {
        let sql = sql.to_owned();
        let max_rows = self.config.max_rows;
        let token = cancel.token.clone();
        let work = self.with_connection(move |connection| {
            let mut output = QueryOutput::default();
            let result = fill_batch(connection, &sql, max_rows, &token, &mut output);
            output.open_transaction = Some(!connection.is_autocommit());
            Ok((output, result.map_err(|error| map_error(&error))))
        });
        tokio::pin!(work);
        let (collected, result) = tokio::select! {
            result = &mut work => result,
            () = cancel.token.cancelled() => {
                self.interrupt.interrupt();
                work.await
            }
        }?;
        *output = collected;
        result
    }

    pub fn close(self) {
        drop(self);
    }

    /// `main` and any attached databases.
    pub async fn list_databases(&mut self) -> Result<Vec<String>, QueryError> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare("select name from pragma_database_list")?;
            let names = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(names.into_iter().filter(|name| name != "temp").collect())
        })
        .await
    }

    pub async fn list_objects(&mut self, database: &str) -> Result<Vec<CatalogObject>, QueryError> {
        let schema = Dialect::Sqlite.quote_identifier(database);
        let database = database.to_owned();
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(&format!(
                "select type, name from {schema}.sqlite_schema \
                 where type in ('table', 'view') and name not like 'sqlite\\_%' escape '\\' \
                 order by name"
            ))?;
            let objects = statement
                .query_map([], |row| {
                    let kind = if row.get::<_, String>(0)? == "view" {
                        ObjectKind::View
                    } else {
                        ObjectKind::Table
                    };
                    Ok(CatalogObject {
                        schema: database.clone(),
                        name: row.get(1)?,
                        kind,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(objects)
        })
        .await
    }

    pub async fn list_columns(
        &mut self,
        database: &str,
        object: &CatalogObject,
    ) -> Result<Vec<ColumnInfo>, QueryError> {
        let database = database.to_owned();
        let name = object.name.clone();
        self.with_connection(move |connection| {
            // A schema prefix on the table-valued pragma is ignored; the argument is not.
            let mut statement = connection
                .prepare("select name, type, \"notnull\" from pragma_table_info(?1, ?2)")?;
            let columns = statement
                .query_map([&name, &database], |row| {
                    Ok(ColumnInfo {
                        name: row.get(0)?,
                        type_name: row.get(1)?,
                        nullable: row.get::<_, i64>(2)? == 0,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(columns)
        })
        .await
    }

    pub async fn list_all_columns(
        &mut self,
        database: &str,
    ) -> Result<Vec<crate::SchemaColumn>, QueryError> {
        let schema = Dialect::Sqlite.quote_identifier(database);
        let database = database.to_owned();
        self.with_connection(move |connection| {
            let mut tables = connection.prepare(&format!(
                "select name from {schema}.sqlite_schema \
                 where type in ('table', 'view') and name not like 'sqlite\\_%' escape '\\' \
                 order by name"
            ))?;
            let names = tables
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut info = connection.prepare(
                "select name, type, \"notnull\" from pragma_table_info(?1, ?2) order by cid",
            )?;
            let mut columns = Vec::new();
            for table in names {
                // A view over a dropped table fails here; skip it, keep the rest.
                let rows = info.query_map([&table, &database], |row| {
                    Ok(ColumnInfo {
                        name: row.get(0)?,
                        type_name: row.get(1)?,
                        nullable: row.get::<_, i64>(2)? == 0,
                    })
                });
                let Ok(rows) = rows.and_then(Iterator::collect::<Result<Vec<_>, _>>) else {
                    continue;
                };
                columns.extend(rows.into_iter().map(|column| crate::SchemaColumn {
                    schema: database.clone(),
                    table: table.clone(),
                    column,
                }));
            }
            Ok(columns)
        })
        .await
    }

    /// The `create` statement SQLite stored for the table or view.
    pub async fn object_definition(
        &mut self,
        database: &str,
        object: &CatalogObject,
    ) -> Result<Option<String>, QueryError> {
        let schema = Dialect::Sqlite.quote_identifier(database);
        let name = object.name.clone();
        self.with_connection(move |connection| {
            let mut statement = connection.prepare(&format!(
                "select sql from {schema}.sqlite_schema where name = ?1"
            ))?;
            let mut rows = statement.query([&name])?;
            let definition = match rows.next()? {
                Some(row) => row
                    .get::<_, Option<String>>(0)?
                    .map(|sql| format!("{sql};\n")),
                None => None,
            };
            Ok(definition)
        })
        .await
    }

    async fn with_connection<T: Send + 'static>(
        &self,
        work: impl FnOnce(&rusqlite::Connection) -> Result<T, rusqlite::Error> + Send + 'static,
    ) -> Result<T, QueryError> {
        let connection = self.connection.clone();
        blocking(move || {
            // A panic in an earlier batch cannot leave SQLite's own state inconsistent.
            let connection = connection.lock().unwrap_or_else(PoisonError::into_inner);
            work(&connection).map_err(|error| map_error(&error))
        })
        .await
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, QueryError> + Send + 'static,
) -> Result<T, QueryError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| QueryError::ConnectionLost(format!("SQLite task failed: {error}")))?
}

#[cfg(test)]
fn run_batch(
    connection: &rusqlite::Connection,
    sql: &str,
    max_rows: usize,
    cancel: &CancellationToken,
) -> Result<QueryOutput, rusqlite::Error> {
    let mut output = QueryOutput::default();
    fill_batch(connection, sql, max_rows, cancel, &mut output)?;
    Ok(output)
}

/// Fills `output` statement by statement, so an error keeps the earlier results.
fn fill_batch(
    connection: &rusqlite::Connection,
    sql: &str,
    max_rows: usize,
    cancel: &CancellationToken,
    output: &mut QueryOutput,
) -> Result<(), rusqlite::Error> {
    let mut batch = Batch::new(connection, sql);
    while let Some(mut statement) = batch.next()? {
        // An interrupt only stops a running statement; one that has not started checks here.
        if cancel.is_cancelled() {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
                None,
            ));
        }
        if statement.column_count() == 0 {
            // `changes()` keeps the last DML count through DDL; the total does not.
            let before = connection.total_changes();
            statement.raw_execute()?;
            *output.rows_affected.get_or_insert(0) += connection.total_changes() - before;
            continue;
        }
        let columns: Vec<Column> = statement
            .columns()
            .map(|column| Column {
                name: column.name().to_owned(),
                type_name: column.decl_type().unwrap_or_default().to_owned(),
            })
            .collect();
        let width = columns.len();
        let mut result_set = ResultSet {
            columns,
            ..ResultSet::default()
        };
        let read_only = statement.readonly();
        let mut rows = statement.raw_query();
        while let Some(row) = rows.next()? {
            if result_set.is_full(max_rows) {
                result_set.truncated = true;
                // Stepping is the work; a statement that only reads can stop here.
                if read_only {
                    break;
                }
                continue;
            }
            let values = (0..width)
                .map(|index| row.get_ref(index).map(to_value))
                .collect::<Result<Vec<_>, _>>()?;
            result_set.push_row(values);
        }
        output.result_sets.push(Arc::new(result_set));
    }
    Ok(())
}

fn to_value(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(number) => Value::Int(number),
        ValueRef::Real(number) => Value::Float(number),
        ValueRef::Text(bytes) => Value::Text(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => Value::Bytes(bytes.to_vec()),
    }
}

fn map_error(error: &rusqlite::Error) -> QueryError {
    match error.sqlite_error_code() {
        Some(ErrorCode::OperationInterrupted) => QueryError::Cancelled,
        Some(_) | None => QueryError::Server(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> rusqlite::Connection {
        rusqlite::Connection::open_in_memory().unwrap()
    }

    #[test]
    fn runs_a_batch_with_result_sets_and_changes() {
        let connection = memory();
        let output = run_batch(
            &connection,
            "create table t (id integer primary key, name text, score real, data blob);
             insert into t values (1, 'a', 1.5, x'01ff'), (2, null, null, null);
             select * from t order by id;
             update t set name = 'b';
             create index t_name on t (name);
             select count(*) as n from t;",
            10,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(output.rows_affected, Some(4), "DDL after DML adds nothing");
        assert_eq!(output.result_sets.len(), 2);
        let first = &output.result_sets[0];
        assert_eq!(first.columns[0].type_name, "INTEGER");
        assert_eq!(
            first.rows[0],
            vec![
                Value::Int(1),
                Value::Text("a".to_owned()),
                Value::Float(1.5),
                Value::Bytes(vec![1, 255]),
            ]
        );
        assert_eq!(first.rows[1][1], Value::Null);
        assert_eq!(output.result_sets[1].rows, vec![vec![Value::Int(2)]]);
    }

    #[test]
    fn truncates_at_the_row_limit() {
        let connection = memory();
        let output = run_batch(
            &connection,
            "with recursive n(i) as (select 1 union all select i + 1 from n where i < 25) select i from n",
            10,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(output.result_sets[0].rows.len(), 10);
        assert!(output.result_sets[0].truncated);
    }

    #[test]
    fn lists_every_column_in_one_call() {
        let path = std::env::temp_dir().join(format!("querent-test-{}.db", std::process::id()));
        {
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "create table a (id integer not null, name text);
                     create view v as select name from a;
                     create table gone (x int);
                     create view broken as select x from gone;
                     drop table gone;",
                )
                .unwrap();
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let columns = runtime.block_on(async {
            let mut connection = SqliteConnection::connect(SqliteConfig::new(&path))
                .await
                .unwrap();
            connection.list_all_columns("main").await.unwrap()
        });
        std::fs::remove_file(&path).unwrap();
        let names: Vec<(String, String, bool)> = columns
            .into_iter()
            .map(|entry| (entry.table, entry.column.name, entry.column.nullable))
            .collect();
        assert_eq!(
            names,
            vec![
                ("a".to_owned(), "id".to_owned(), false),
                ("a".to_owned(), "name".to_owned(), true),
                ("v".to_owned(), "name".to_owned(), true),
            ]
        );
    }

    #[test]
    fn interrupt_reads_as_cancelled() {
        let connection = memory();
        let interrupt = connection.get_interrupt_handle();
        let worker = std::thread::spawn(move || {
            run_batch(
                &connection,
                "with recursive n(i) as (select 1 union all select i + 1 from n) select count(*) from n",
                10,
                &CancellationToken::new(),
            )
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        interrupt.interrupt();
        let error = worker.join().unwrap().unwrap_err();
        assert!(matches!(map_error(&error), QueryError::Cancelled));
    }
}
