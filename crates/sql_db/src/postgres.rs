//! PostgreSQL driver on `tokio-postgres` with TLS from `native-tls`.
//!
//! Batches run through the simple query protocol, so a buffer with several
//! statements works as it does on SQL Server. Values arrive as the server's
//! text; a single statement is also prepared first so its column types turn
//! that text into numbers, booleans, dates and bytes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{StreamExt as _, pin_mut};
use postgres_native_tls::MakeTlsConnector;
use tokio::sync::mpsc;
use tokio_postgres::config::SslMode;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::Type;
use tokio_postgres::{AsyncMessage, CancelToken, Client, SimpleQueryMessage};
use tokio_util::sync::CancellationToken;

use crate::catalog::Dialect;
use crate::{
    Authentication, CancelHandle, CatalogObject, Column, ColumnClause, ColumnExtra, ColumnInfo,
    ConstraintKind, Encryption, ObjectKind, QueryError, QueryOutput, ResultSet, TableConstraint,
    TableDetails, TableIndex, Value,
};

pub const DEFAULT_PORT: u16 = 5432;
const APPLICATION_NAME: &str = "querent";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const TRANSACTION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const QUERY_CANCELED: &str = "57014";
const CANCEL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const AUTH_ERRORS: [&str; 2] = ["28P01", "28000"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgConfig {
    pub host: String,
    /// `None` means 5432.
    pub port: Option<u16>,
    /// Empty means the server's default, a database named like the user.
    pub database: String,
    /// Only `Authentication::Password` is accepted.
    pub authentication: Authentication,
    /// `Optional` is libpq's `prefer`, `Mandatory` and `Strict` are `require`.
    pub encryption: Encryption,
    /// Skips certificate and host name checks; otherwise TLS verifies like `verify-full`.
    pub trust_server_certificate: bool,
    /// Rows kept per result set; the rest are skipped and the set is marked truncated.
    pub max_rows: usize,
}

impl PgConfig {
    #[must_use]
    pub fn new(host: impl Into<String>, database: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            port: None,
            database: database.into(),
            authentication: Authentication::Password {
                user: String::new(),
                password: String::new(),
            },
            encryption: Encryption::Mandatory,
            trust_server_certificate: false,
            max_rows: 50_000,
        }
    }

    #[must_use]
    pub fn server_label(&self) -> String {
        format!("{}:{}", self.host, self.port.unwrap_or(DEFAULT_PORT))
    }

    fn with_database(&self, database: &str) -> Self {
        Self {
            database: database.to_owned(),
            ..self.clone()
        }
    }
}

/// One open session: the client, the cancel token for it, and the notices
/// (`RAISE NOTICE`) the background connection task has received.
struct Session {
    client: Client,
    cancel_token: CancelToken,
    notices: mpsc::UnboundedReceiver<String>,
    database: String,
}

pub struct PgConnection {
    config: PgConfig,
    tls: MakeTlsConnector,
    session: Session,
    /// Sessions on other databases, opened for the object explorer: a PostgreSQL
    /// session cannot read another database's catalog.
    catalog_sessions: HashMap<String, Session>,
}

impl std::fmt::Debug for PgConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PgConnection")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl PgConnection {
    #[tracing::instrument(skip_all, fields(server = %config.server_label(), database = %config.database))]
    pub async fn connect(config: PgConfig) -> Result<Self, QueryError> {
        let tls = tls_connector(&config)?;
        let session = open_session(&config, &tls).await?;
        Ok(Self {
            config,
            tls,
            session,
            catalog_sessions: HashMap::new(),
        })
    }

    #[must_use]
    pub fn config(&self) -> &PgConfig {
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
        let Self {
            session,
            tls,
            config,
            ..
        } = self;
        // Cancelling inside a transaction block would abort it, so only a lone
        // read in autocommit is stopped at the limit instead of drained.
        let stop_at_limit = config.max_rows != usize::MAX
            && sql_text::read_only::is_read_only_query(sql)
            && !in_transaction_block(&session.client).await;
        let stop = CancellationToken::new();
        let result = {
            let limit = Limit {
                max_rows: config.max_rows,
                stop: stop_at_limit.then_some(&stop),
            };
            let work = run_batch(&session.client, sql, &limit, cancel, output);
            pin_mut!(work);
            tokio::select! {
                result = &mut work => result,
                () = async { tokio::select! {
                    () = cancel.token.cancelled() => {}
                    () = stop.cancelled() => {}
                } } => {
                    send_cancel(&session.cancel_token, tls).await;
                    work.await
                }
            }
        };
        output.messages.extend(drain(&mut session.notices));
        output.open_transaction = transaction_state(&session.client).await;
        match result {
            Ok(()) => Ok(()),
            Err(error)
                if stop.is_cancelled() && !cancel.token.is_cancelled() && is_cancel(&error) =>
            {
                Ok(())
            }
            // `statement_timeout` also raises 57014; only a user cancel reads as cancelled.
            Err(error) => {
                let mapped = map_error(&error);
                if matches!(mapped, QueryError::Cancelled) && !cancel.token.is_cancelled() {
                    Err(QueryError::Server(error.to_string()))
                } else {
                    Err(located(mapped, &error, sql))
                }
            }
        }
    }

    /// Dropping the client sends Terminate and ends the connection task.
    pub fn close(self) {
        drop(self);
    }

    pub async fn list_databases(&mut self) -> Result<Vec<String>, QueryError> {
        let rows = self
            .session
            .client
            .query(
                "select datname from pg_database \
                 where not datistemplate and has_database_privilege(datname, 'CONNECT') \
                 order by datname",
                &[],
            )
            .await
            .map_err(|error| map_error(&error))?;
        Ok(rows.iter().map(|row| row.get::<_, String>(0)).collect())
    }

    pub async fn list_objects(&mut self, database: &str) -> Result<Vec<CatalogObject>, QueryError> {
        let client = &self.catalog_session(database).await?.client;
        let rows = client
            .query(
                "select n.nspname::text, c.relname::text, \
                     case when c.relkind in ('v', 'm') then 'V' else 'T' end \
                 from pg_class c join pg_namespace n on n.oid = c.relnamespace \
                 where c.relkind in ('r', 'p', 'f', 'v', 'm') and n.nspname <> 'information_schema' \
                   and n.nspname not like 'pg\\_%' \
                 union all \
                 select n.nspname::text, p.proname::text, \
                     case p.prokind when 'p' then 'P' else 'F' end \
                 from pg_proc p join pg_namespace n on n.oid = p.pronamespace \
                 where p.prokind in ('f', 'p') and n.nspname <> 'information_schema' \
                   and n.nspname not like 'pg\\_%' \
                   and not exists (select 1 from pg_depend d \
                                   where d.objid = p.oid and d.deptype = 'e') \
                 order by 1, 2, 3",
                &[],
            )
            .await
            .map_err(|error| map_error(&error))?;
        let mut objects: Vec<CatalogObject> = rows
            .iter()
            .filter_map(|row| {
                let kind = match row.get::<_, String>(2).as_str() {
                    "T" => ObjectKind::Table,
                    "V" => ObjectKind::View,
                    "P" => ObjectKind::Procedure,
                    "F" => ObjectKind::Function,
                    _ => return None,
                };
                Some(CatalogObject {
                    schema: row.get(0),
                    name: row.get(1),
                    kind,
                })
            })
            .collect();
        // Overloaded functions share a name; with kind in the sort they are adjacent.
        objects.dedup();
        Ok(objects)
    }

    /// Defaults, identity, keys, checks and indexes of a table, in the server's own wording.
    pub async fn table_details(
        &mut self,
        database: &str,
        object: &CatalogObject,
    ) -> Result<TableDetails, QueryError> {
        let name = object.qualified_name(Dialect::Postgres);
        let client = &self.catalog_session(database).await?.client;
        let extras = client
            .query(
                // A `serial` column owns its sequence, which the script would lack.
                "select a.attname::text, pg_get_expr(d.adbin, d.adrelid), \
                 case when a.attidentity = '' and pg_get_serial_sequence($1, a.attname::text) is not null \
                   then 'd' else a.attidentity::text end, a.attgenerated::text \
                 from pg_attribute a \
                 left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum \
                 where a.attrelid = to_regclass($1) and a.attnum > 0 and not a.attisdropped \
                   and (d.adbin is not null or a.attidentity <> '') \
                 order by a.attnum",
                &[&name],
            )
            .await
            .map_err(|error| map_error(&error))?;
        let constraints = client
            .query(
                "select conname::text, contype::text, pg_get_constraintdef(oid) \
                 from pg_constraint \
                 where conrelid = to_regclass($1) and contype in ('p', 'u', 'f', 'c') \
                 order by array_position(array['p', 'u', 'f', 'c'], contype::text), conname",
                &[&name],
            )
            .await
            .map_err(|error| map_error(&error))?;
        let indexes = client
            .query(
                // An index behind a primary key or unique constraint is scripted as that constraint.
                "select c.relname::text, i.indisunique, pg_get_indexdef(i.indexrelid) \
                 from pg_index i join pg_class c on c.oid = i.indexrelid \
                 where i.indrelid = to_regclass($1) and not exists ( \
                   select 1 from pg_constraint k \
                   where k.conindid = i.indexrelid and k.contype in ('p', 'u')) \
                 order by c.relname",
                &[&name],
            )
            .await
            .map_err(|error| map_error(&error))?;
        Ok(TableDetails {
            incomplete: false,
            column_extras: extras
                .iter()
                .filter_map(|row| {
                    let default: Option<String> = row.get(1);
                    let identity: String = row.get(2);
                    let generated: String = row.get(3);
                    Some(ColumnExtra {
                        column: row.get(0),
                        clause: ColumnClause::AfterType(column_clause(
                            default.as_deref(),
                            &identity,
                            &generated,
                        )?),
                    })
                })
                .collect(),
            constraints: constraints
                .iter()
                .filter_map(|row| {
                    let kind: String = row.get(1);
                    Some(TableConstraint {
                        name: row.get(0),
                        kind: match kind.as_str() {
                            "p" => ConstraintKind::PrimaryKey,
                            "u" => ConstraintKind::Unique,
                            "f" => ConstraintKind::ForeignKey,
                            "c" => ConstraintKind::Check,
                            _ => return None,
                        },
                        definition: row.get(2),
                    })
                })
                .collect(),
            indexes: indexes
                .iter()
                .map(|row| {
                    let unique: bool = row.get(1);
                    let statement: String = row.get(2);
                    TableIndex {
                        name: row.get(0),
                        summary: index_summary(unique, &statement),
                        statement,
                    }
                })
                .collect(),
        })
    }

    pub async fn list_columns(
        &mut self,
        database: &str,
        object: &CatalogObject,
    ) -> Result<Vec<ColumnInfo>, QueryError> {
        let name = object.qualified_name(Dialect::Postgres);
        let client = &self.catalog_session(database).await?.client;
        let rows = client
            .query(
                "select a.attname::text, format_type(a.atttypid, a.atttypmod), not a.attnotnull \
                 from pg_attribute a \
                 where a.attrelid = to_regclass($1) and a.attnum > 0 and not a.attisdropped \
                 order by a.attnum",
                &[&name],
            )
            .await
            .map_err(|error| map_error(&error))?;
        Ok(rows
            .iter()
            .map(|row| ColumnInfo {
                name: row.get(0),
                type_name: row.get(1),
                nullable: row.get(2),
            })
            .collect())
    }

    pub async fn list_all_columns(
        &mut self,
        database: &str,
    ) -> Result<Vec<crate::SchemaColumn>, QueryError> {
        let client = &self.catalog_session(database).await?.client;
        let rows = client
            .query(
                "select n.nspname::text, c.relname::text, a.attname::text, \
                     format_type(a.atttypid, a.atttypmod), not a.attnotnull \
                 from pg_attribute a \
                 join pg_class c on c.oid = a.attrelid \
                 join pg_namespace n on n.oid = c.relnamespace \
                 where c.relkind in ('r', 'p', 'f', 'v', 'm') and a.attnum > 0 \
                   and not a.attisdropped and n.nspname <> 'information_schema' \
                   and n.nspname not like 'pg\\_%' \
                 order by 1, 2, a.attnum",
                &[],
            )
            .await
            .map_err(|error| map_error(&error))?;
        Ok(rows
            .iter()
            .map(|row| crate::SchemaColumn {
                schema: row.get(0),
                table: row.get(1),
                column: ColumnInfo {
                    name: row.get(2),
                    type_name: row.get(3),
                    nullable: row.get(4),
                },
            })
            .collect())
    }

    /// `create or replace` source of a view, function or procedure; `None` for tables.
    pub async fn object_definition(
        &mut self,
        database: &str,
        object: &CatalogObject,
    ) -> Result<Option<String>, QueryError> {
        let name = object.qualified_name(Dialect::Postgres);
        let client = &self.catalog_session(database).await?.client;
        let result = match object.kind {
            ObjectKind::Table => return Ok(None),
            ObjectKind::View => client
                .query_opt(
                    "select c.relkind = 'm', pg_get_viewdef(c.oid, true) \
                     from pg_class c where c.oid = to_regclass($1)",
                    &[&name],
                )
                .await
                .map(|row| {
                    let row = row?;
                    let body = row.get::<_, Option<String>>(1)?;
                    let create = if row.get::<_, bool>(0) {
                        "create materialized view"
                    } else {
                        "create or replace view"
                    };
                    Some(format!("{create} {name} as\n{body}\n"))
                }),
            ObjectKind::Procedure | ObjectKind::Function => client
                .query(
                    "select pg_get_functiondef(p.oid) from pg_proc p \
                     join pg_namespace n on n.oid = p.pronamespace \
                     where n.nspname = $1 and p.proname = $2 and p.prokind in ('f', 'p') \
                     order by p.oid",
                    &[&object.schema, &object.name],
                )
                .await
                .map(|rows| {
                    let bodies: Vec<String> = rows
                        .iter()
                        .map(|row| format!("{};\n", row.get::<_, String>(0).trim_end()))
                        .collect();
                    (!bodies.is_empty()).then(|| bodies.join("\n"))
                }),
        };
        result.map_err(|error| map_error(&error))
    }

    /// The main session when it is on `database`, else a catalog session opened for it.
    async fn catalog_session(&mut self, database: &str) -> Result<&Session, QueryError> {
        if self.session.database == database {
            return Ok(&self.session);
        }
        // A session dropped by a server restart or idle timeout is reopened.
        if self
            .catalog_sessions
            .get(database)
            .is_some_and(|session| session.client.is_closed())
        {
            self.catalog_sessions.remove(database);
        }
        if !self.catalog_sessions.contains_key(database) {
            let config = self.config.with_database(database);
            let session = open_session(&config, &self.tls).await?;
            self.catalog_sessions.insert(database.to_owned(), session);
        }
        self.catalog_sessions
            .get(database)
            .ok_or_else(|| QueryError::ConnectionLost("catalog session missing".to_owned()))
    }
}

fn tls_connector(config: &PgConfig) -> Result<MakeTlsConnector, QueryError> {
    let connector = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(config.trust_server_certificate)
        .danger_accept_invalid_hostnames(config.trust_server_certificate)
        .build()
        .map_err(|error| QueryError::Connect(format!("TLS setup: {error}")))?;
    Ok(MakeTlsConnector::new(connector))
}

async fn open_session(config: &PgConfig, tls: &MakeTlsConnector) -> Result<Session, QueryError> {
    let Authentication::Password { user, password } = &config.authentication else {
        return Err(QueryError::Auth(
            "PostgreSQL connections sign in with a user name and password".to_owned(),
        ));
    };
    let mut pg_config = tokio_postgres::Config::new();
    pg_config
        .host(&config.host)
        .port(config.port.unwrap_or(DEFAULT_PORT))
        .user(user)
        .application_name(APPLICATION_NAME)
        .connect_timeout(CONNECT_TIMEOUT)
        .ssl_mode(match config.encryption {
            Encryption::Optional => SslMode::Prefer,
            Encryption::Mandatory | Encryption::Strict => SslMode::Require,
        });
    if !password.is_empty() {
        pg_config.password(password);
    }
    if !config.database.is_empty() {
        pg_config.dbname(&config.database);
    }
    let (client, mut connection) = pg_config
        .connect(tls.clone())
        .await
        .map_err(|error| connect_error(config, &error))?;
    let (notice_sender, notices) = mpsc::unbounded_channel();
    // Drives the socket; it ends when the client is dropped and the server closes.
    tokio::spawn(async move {
        loop {
            match std::future::poll_fn(|context| connection.poll_message(context)).await {
                Some(Ok(AsyncMessage::Notice(notice))) => {
                    if notice_sender.send(notice.message().to_owned()).is_err() {
                        break;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    tracing::debug!(%error, "postgres connection ended");
                    break;
                }
                None => break,
            }
        }
    });
    let database = client
        .query_one("select current_database()::text", &[])
        .await
        .map_err(|error| map_error(&error))?
        .get(0);
    Ok(Session {
        cancel_token: client.cancel_token(),
        client,
        notices,
        database,
    })
}

async fn send_cancel(token: &CancelToken, tls: &MakeTlsConnector) {
    let request = token.cancel_query(tls.clone());
    match tokio::time::timeout(CANCEL_REQUEST_TIMEOUT, request).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::debug!(%error, "cancel request failed"),
        Err(_) => tracing::debug!("cancel request timed out"),
    }
}

/// `now()` is fixed at transaction start, so it differs from the statement's time inside a block.
async fn in_transaction_block(client: &Client) -> bool {
    match client
        .simple_query("select now() <> statement_timestamp()")
        .await
    {
        Ok(messages) => messages.iter().any(
            |message| matches!(message, SimpleQueryMessage::Row(row) if row.get(0) == Some("t")),
        ),
        // Unknown: assume a block, which only means draining instead of stopping.
        Err(_) => true,
    }
}

/// Whether a transaction block is open after a run; `None` when the server did not say in time.
async fn transaction_state(client: &Client) -> Option<bool> {
    // Past the run's cancel, so a dead connection must not hold the tab here.
    let probe = client.simple_query("select now() <> statement_timestamp()");
    match tokio::time::timeout(TRANSACTION_PROBE_TIMEOUT, probe).await {
        Ok(Ok(messages)) => Some(messages.iter().any(
            |message| matches!(message, SimpleQueryMessage::Row(row) if row.get(0) == Some("t")),
        )),
        // An aborted block refuses every statement and still needs a rollback.
        Ok(Err(error)) if error.code() == Some(&SqlState::IN_FAILED_SQL_TRANSACTION) => Some(true),
        // A late cancel can hit the probe; that says nothing about a transaction.
        Ok(Err(_)) | Err(_) => None,
    }
}

/// The row cap, and the token that asks `execute` to stop the server once it is reached.
struct Limit<'a> {
    max_rows: usize,
    stop: Option<&'a CancellationToken>,
}

/// Fills `output` as it reads, so rows read before a stop at the limit are kept.
async fn run_batch(
    client: &Client,
    sql: &str,
    limit: &Limit<'_>,
    cancel: &CancelHandle,
    output: &mut QueryOutput,
) -> Result<(), tokio_postgres::Error> {
    // Only a single statement is prepared: a failed prepare inside an open
    // transaction would abort it before the batch runs. A batch's values stay text.
    let types: Option<Vec<Type>> =
        if sql_text::split(sql, sql_text::Delimiters::Semicolon).len() == 1 {
            match client.prepare(sql).await {
                Ok(statement) => Some(
                    statement
                        .columns()
                        .iter()
                        .map(|column| column.type_().clone())
                        .collect(),
                ),
                Err(error) if is_cancel(&error) => return Err(error),
                // The batch itself reports the same error with its position.
                Err(_) => None,
            }
        } else {
            None
        };
    if cancel.token.is_cancelled() {
        // The cancel request reached an idle backend; run nothing.
        output
            .messages
            .push("query cancelled before it started".to_owned());
        return Ok(());
    }
    let stream = client.simple_query_raw(sql).await?;
    pin_mut!(stream);
    let mut current: Option<ResultSet> = None;
    while let Some(message) = stream.next().await {
        match message? {
            SimpleQueryMessage::RowDescription(columns) => {
                let column_types = types
                    .as_deref()
                    .filter(|types| types.len() == columns.len());
                current = Some(ResultSet {
                    columns: columns
                        .iter()
                        .enumerate()
                        .map(|(index, column)| Column {
                            name: column.name().to_owned(),
                            type_name: column_types
                                .and_then(|types| types.get(index))
                                .map_or_else(|| "text".to_owned(), |ty| ty.name().to_owned()),
                        })
                        .collect(),
                    ..ResultSet::default()
                });
            }
            SimpleQueryMessage::Row(row) => {
                let Some(result_set) = current.as_mut() else {
                    continue;
                };
                if result_set.is_full(limit.max_rows) {
                    result_set.truncated = true;
                    if let Some(stop) = limit.stop {
                        // Keep what was read; the stream then ends in a cancel error.
                        output.result_sets.extend(current.take().map(Arc::new));
                        stop.cancel();
                    }
                    continue;
                }
                let column_types = types.as_deref().filter(|types| types.len() == row.len());
                let values = (0..row.len())
                    .map(|index| {
                        let ty = column_types.and_then(|types| types.get(index));
                        row.get(index)
                            .map_or(Value::Null, |text| text_value(text, ty))
                    })
                    .collect();
                result_set.push_row(values);
            }
            SimpleQueryMessage::CommandComplete(count) => match current.take() {
                Some(result_set) => output.result_sets.push(Arc::new(result_set)),
                // The count of a result stopped at the limit is not rows affected.
                None if limit.stop.is_some_and(CancellationToken::is_cancelled) => {}
                None => *output.rows_affected.get_or_insert(0) += count,
            },
            _ => {}
        }
    }
    Ok(())
}

/// Turns the server's text for a value into a typed value when the type is known.
fn text_value(text: &str, ty: Option<&Type>) -> Value {
    let Some(ty) = ty else {
        return Value::Text(text.to_owned());
    };
    let as_text = || Value::Text(text.to_owned());
    match *ty {
        Type::BOOL => Value::Bool(text == "t"),
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => {
            text.parse().map_or_else(|_| as_text(), Value::Int)
        }
        Type::FLOAT4 | Type::FLOAT8 => text.parse().map_or_else(|_| as_text(), Value::Float),
        Type::NUMERIC => Value::Decimal(text.to_owned()),
        Type::DATE => Value::Date(text.to_owned()),
        Type::TIME | Type::TIMETZ => Value::Time(text.to_owned()),
        Type::TIMESTAMP | Type::TIMESTAMPTZ => Value::Timestamp(text.to_owned()),
        Type::BYTEA => decode_bytea(text).map_or_else(as_text, Value::Bytes),
        _ => as_text(),
    }
}

/// `\x0102ff`, the default `bytea_output = hex` form.
fn decode_bytea(text: &str) -> Option<Vec<u8>> {
    let hex = text.strip_prefix("\\x")?;
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(hex.get(index..index + 2)?, 16).ok())
        .collect()
}

fn is_cancel(error: &tokio_postgres::Error) -> bool {
    error
        .as_db_error()
        .is_some_and(|database_error| database_error.code().code() == QUERY_CANCELED)
}

fn drain(notices: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
    let mut messages = Vec::new();
    while let Ok(message) = notices.try_recv() {
        messages.push(message);
    }
    messages
}

/// Adds the line PostgreSQL points at: it reports a character position in the sent text.
fn located(mapped: QueryError, error: &tokio_postgres::Error, sql: &str) -> QueryError {
    let position = error
        .as_db_error()
        .and_then(tokio_postgres::error::DbError::position);
    match (mapped, position) {
        (
            QueryError::Server(message),
            Some(tokio_postgres::error::ErrorPosition::Original(position)),
        ) => {
            let characters = usize::try_from(*position).unwrap_or(1).saturating_sub(1);
            let line = sql
                .chars()
                .take(characters)
                .filter(|character| *character == '\n')
                .count()
                + 1;
            QueryError::ServerAt { message, line }
        }
        (mapped, _) => mapped,
    }
}

fn map_error(error: &tokio_postgres::Error) -> QueryError {
    if let Some(database_error) = error.as_db_error() {
        if database_error.code().code() == QUERY_CANCELED {
            return QueryError::Cancelled;
        }
        let lines: Vec<String> = [
            Some(format!(
                "{}: {}",
                database_error.severity(),
                database_error.message()
            )),
            database_error
                .detail()
                .map(|detail| format!("Detail: {detail}")),
            database_error.hint().map(|hint| format!("Hint: {hint}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        return QueryError::Server(lines.join("\n"));
    }
    QueryError::ConnectionLost(error.to_string())
}

fn connect_error(config: &PgConfig, error: &tokio_postgres::Error) -> QueryError {
    let server = config.server_label();
    if let Some(database_error) = error.as_db_error() {
        let message = format!("{server}: {}", database_error.message());
        if AUTH_ERRORS.contains(&database_error.code().code()) {
            return QueryError::Auth(message);
        }
        return QueryError::Connect(message);
    }
    let text = error.to_string();
    let source = std::error::Error::source(error)
        .map(|source| format!(": {source}"))
        .unwrap_or_default();
    let hint = if source.contains("certificate") && !config.trust_server_certificate {
        " The server's certificate is not trusted; tick \"Trust the server certificate\" if you trust this server."
    } else if text.contains("does not support TLS") || source.contains("does not support TLS") {
        ". Set Encryption to Prefer to connect without encryption."
    } else {
        ""
    };
    QueryError::Connect(format!("{server}: {text}{source}{hint}"))
}

/// What follows a column's type: an identity, a generated expression or a default.
fn column_clause(default: Option<&str>, identity: &str, generated: &str) -> Option<String> {
    match (identity, generated, default) {
        ("a", _, _) => Some("generated always as identity".to_owned()),
        ("d", _, _) => Some("generated by default as identity".to_owned()),
        (_, "s", Some(expression)) => Some(format!("generated always as ({expression}) stored")),
        (_, "v", Some(expression)) => Some(format!("generated always as ({expression}) virtual")),
        (_, _, Some(expression)) => Some(format!("default {expression}")),
        (_, _, None) => None,
    }
}

/// `unique index btree (a, b)` from `CREATE UNIQUE INDEX i ON public.t USING btree (a, b)`.
fn index_summary(unique: bool, statement: &str) -> String {
    let unique = if unique { "unique " } else { "" };
    let columns = statement
        .split_once(" USING ")
        .map_or("", |(_, columns)| columns);
    format!("{unique}index {columns}").trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_text_values() {
        assert_eq!(text_value("t", Some(&Type::BOOL)), Value::Bool(true));
        assert_eq!(text_value("42", Some(&Type::INT8)), Value::Int(42));
        assert_eq!(text_value("1.5", Some(&Type::FLOAT8)), Value::Float(1.5));
        assert_eq!(
            text_value("1.50", Some(&Type::NUMERIC)),
            Value::Decimal("1.50".to_owned())
        );
        assert_eq!(
            text_value("\\x01ff", Some(&Type::BYTEA)),
            Value::Bytes(vec![1, 255])
        );
        assert_eq!(
            text_value("{1,2}", Some(&Type::INT4_ARRAY)),
            Value::Text("{1,2}".to_owned())
        );
        assert_eq!(text_value("7", None), Value::Text("7".to_owned()));
    }

    #[test]
    fn rejects_odd_bytea() {
        assert_eq!(decode_bytea("\\x0"), None);
        assert_eq!(decode_bytea("abc"), None);
    }
}
