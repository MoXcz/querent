//! One connection type for the app, dispatching to the driver for the database.

use crate::catalog::Dialect;
use crate::postgres::{PgConfig, PgConnection};
use crate::snowflake::{SnowflakeConfig, SnowflakeConnection};
use crate::sqlite::{SqliteConfig, SqliteConnection};
use crate::{
    CancelHandle, CatalogObject, ColumnInfo, QueryError, QueryOutput, TdsConfig, TdsConnection,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionConfig {
    /// SQL Server and Fabric.
    Tds(TdsConfig),
    Postgres(PgConfig),
    Sqlite(SqliteConfig),
    Snowflake(SnowflakeConfig),
}

impl ConnectionConfig {
    #[must_use]
    pub fn database(&self) -> &str {
        match self {
            Self::Tds(config) => &config.database,
            Self::Postgres(config) => &config.database,
            // One file; attached databases are named in the SQL itself.
            Self::Sqlite(_) => "",
            Self::Snowflake(config) => &config.database,
        }
    }

    pub fn set_database(&mut self, database: String) {
        match self {
            Self::Tds(config) => config.database = database,
            Self::Postgres(config) => config.database = database,
            Self::Sqlite(_) => {}
            Self::Snowflake(config) => config.database = database,
        }
    }

    /// PostgreSQL falls back to a database named like the user; TDS needs one.
    #[must_use]
    pub fn requires_database(&self) -> bool {
        match self {
            Self::Tds(_) => true,
            Self::Postgres(_) | Self::Sqlite(_) | Self::Snowflake(_) => false,
        }
    }

    #[must_use]
    pub fn dialect(&self) -> Dialect {
        match self {
            Self::Tds(_) => Dialect::TSql,
            Self::Postgres(_) => Dialect::Postgres,
            Self::Sqlite(_) => Dialect::Sqlite,
            Self::Snowflake(_) => Dialect::Snowflake,
        }
    }
}

#[derive(Debug)]
pub enum Connection {
    // Boxed: both clients are hundreds of bytes and the enum is moved into tasks.
    Tds(Box<TdsConnection>),
    Postgres(Box<PgConnection>),
    Sqlite(SqliteConnection),
    Snowflake(Box<SnowflakeConnection>),
}

impl Connection {
    pub async fn connect(config: ConnectionConfig) -> Result<Self, QueryError> {
        match config {
            ConnectionConfig::Tds(config) => TdsConnection::connect(config)
                .await
                .map(|connection| Self::Tds(Box::new(connection))),
            ConnectionConfig::Postgres(config) => PgConnection::connect(config)
                .await
                .map(|connection| Self::Postgres(Box::new(connection))),
            ConnectionConfig::Sqlite(config) => {
                SqliteConnection::connect(config).await.map(Self::Sqlite)
            }
            ConnectionConfig::Snowflake(config) => SnowflakeConnection::connect(config)
                .await
                .map(|connection| Self::Snowflake(Box::new(connection))),
        }
    }

    /// Whether this connection was opened with `config`, so a tab can reuse it.
    #[must_use]
    pub fn matches(&self, config: &ConnectionConfig) -> bool {
        match (self, config) {
            (Self::Tds(connection), ConnectionConfig::Tds(config)) => connection.config() == config,
            (Self::Postgres(connection), ConnectionConfig::Postgres(config)) => {
                connection.config() == config
            }
            (Self::Sqlite(connection), ConnectionConfig::Sqlite(config)) => {
                connection.config() == config
            }
            (Self::Snowflake(connection), ConnectionConfig::Snowflake(config)) => {
                connection.config() == config
            }
            (
                Self::Tds(_) | Self::Postgres(_) | Self::Sqlite(_) | Self::Snowflake(_),
                ConnectionConfig::Tds(_)
                | ConnectionConfig::Postgres(_)
                | ConnectionConfig::Sqlite(_)
                | ConnectionConfig::Snowflake(_),
            ) => false,
        }
    }

    pub async fn execute(
        &mut self,
        sql: &str,
        cancel: &CancelHandle,
    ) -> Result<QueryOutput, QueryError> {
        match self {
            Self::Tds(connection) => connection.execute(sql, cancel).await,
            Self::Postgres(connection) => connection.execute(sql, cancel).await,
            Self::Sqlite(connection) => connection.execute(sql, cancel).await,
            Self::Snowflake(connection) => connection.execute(sql, cancel).await,
        }
    }

    pub async fn close(self) -> Result<(), QueryError> {
        match self {
            Self::Tds(connection) => connection.close().await,
            Self::Postgres(connection) => {
                connection.close();
                Ok(())
            }
            Self::Sqlite(connection) => {
                connection.close();
                Ok(())
            }
            Self::Snowflake(connection) => {
                connection.close();
                Ok(())
            }
        }
    }

    pub async fn list_databases(&mut self) -> Result<Vec<String>, QueryError> {
        match self {
            Self::Tds(connection) => connection.list_databases().await,
            Self::Postgres(connection) => connection.list_databases().await,
            Self::Sqlite(connection) => connection.list_databases().await,
            Self::Snowflake(connection) => connection.list_databases().await,
        }
    }

    pub async fn list_objects(&mut self, database: &str) -> Result<Vec<CatalogObject>, QueryError> {
        match self {
            Self::Tds(connection) => connection.list_objects(database).await,
            Self::Postgres(connection) => connection.list_objects(database).await,
            Self::Sqlite(connection) => connection.list_objects(database).await,
            Self::Snowflake(connection) => connection.list_objects(database).await,
        }
    }

    pub async fn list_columns(
        &mut self,
        database: &str,
        object: &CatalogObject,
    ) -> Result<Vec<ColumnInfo>, QueryError> {
        match self {
            Self::Tds(connection) => connection.list_columns(database, object).await,
            Self::Postgres(connection) => connection.list_columns(database, object).await,
            Self::Sqlite(connection) => connection.list_columns(database, object).await,
            Self::Snowflake(connection) => connection.list_columns(database, object).await,
        }
    }

    pub async fn object_definition(
        &mut self,
        database: &str,
        object: &CatalogObject,
    ) -> Result<Option<String>, QueryError> {
        match self {
            Self::Tds(connection) => connection.object_definition(database, object).await,
            Self::Postgres(connection) => connection.object_definition(database, object).await,
            Self::Sqlite(connection) => connection.object_definition(database, object).await,
            Self::Snowflake(connection) => connection.object_definition(database, object).await,
        }
    }
}
