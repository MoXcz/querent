//! Database objects for the object explorer, and scripts generated from them.
//!
//! SQL Server queries name the database explicitly (`[db].sys.objects`), so one
//! connection can browse every database on the server or Fabric workspace.

use crate::{CancelHandle, QueryError, ResultSet, TdsConnection, Value};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ObjectKind {
    Table,
    View,
    Procedure,
    Function,
}

impl ObjectKind {
    pub const ALL: [Self; 4] = [Self::Table, Self::View, Self::Procedure, Self::Function];

    #[must_use]
    pub fn plural_label(self) -> &'static str {
        match self {
            Self::Table => "Tables",
            Self::View => "Views",
            Self::Procedure => "Stored Procedures",
            Self::Function => "Functions",
        }
    }

    fn from_type_desc(type_desc: &str) -> Option<Self> {
        match type_desc {
            "USER_TABLE" => Some(Self::Table),
            "VIEW" => Some(Self::View),
            "SQL_STORED_PROCEDURE" => Some(Self::Procedure),
            "SQL_SCALAR_FUNCTION"
            | "SQL_INLINE_TABLE_VALUED_FUNCTION"
            | "SQL_TABLE_VALUED_FUNCTION" => Some(Self::Function),
            _ => None,
        }
    }
}

/// How generated SQL quotes names and limits rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// SQL Server and Fabric: `[name]`, `select top N`.
    TSql,
    /// PostgreSQL: `"name"`, `limit N`.
    Postgres,
    /// SQLite: quotes and limits like PostgreSQL.
    Sqlite,
    /// Snowflake: quotes and limits like PostgreSQL; unquoted names are stored uppercase.
    Snowflake,
}

impl Dialect {
    /// `[name]` with `]` doubled, or `"name"` with `"` doubled.
    #[must_use]
    pub fn quote_identifier(self, name: &str) -> String {
        match self {
            Self::TSql => quote_identifier(name),
            Self::Postgres | Self::Sqlite | Self::Snowflake => {
                format!("\"{}\"", name.replace('"', "\"\""))
            }
        }
    }
}

/// Common reserved words; a table named `order` must be quoted.
const RESERVED_WORDS: [&str; 24] = [
    "select", "from", "where", "order", "group", "by", "user", "table", "join", "on", "and", "or",
    "not", "null", "key", "index", "check", "default", "values", "insert", "update", "delete",
    "to", "end",
];

impl Dialect {
    /// The name as typed in SQL: bare when the database reads it back unchanged,
    /// quoted otherwise (spaces, symbols, or a case the database would fold).
    #[must_use]
    pub fn name_in_sql(self, name: &str) -> String {
        let mut characters = name.chars();
        let first_ok = characters
            .next()
            .is_some_and(|character| character.is_ascii_alphabetic() || character == '_');
        let rest_ok =
            characters.all(|character| character.is_ascii_alphanumeric() || character == '_');
        let case_ok = match self {
            Self::TSql | Self::Sqlite => true,
            Self::Postgres => !name.chars().any(|character| character.is_ascii_uppercase()),
            Self::Snowflake => !name.chars().any(|character| character.is_ascii_lowercase()),
        };
        let reserved = RESERVED_WORDS
            .iter()
            .any(|word| word.eq_ignore_ascii_case(name));
        if first_ok && rest_ok && case_ok && !reserved {
            name.to_owned()
        } else {
            self.quote_identifier(name)
        }
    }
}

/// One column of a table or view, for the schema cache behind completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaColumn {
    pub schema: String,
    pub table: String,
    pub column: ColumnInfo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogObject {
    pub schema: String,
    pub name: String,
    pub kind: ObjectKind,
}

impl CatalogObject {
    /// `[schema].[name]` or `"schema"."name"`.
    #[must_use]
    pub fn qualified_name(&self, dialect: Dialect) -> String {
        format!(
            "{}.{}",
            dialect.quote_identifier(&self.schema),
            dialect.quote_identifier(&self.name)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    /// Declared type as written in DDL, e.g. `nvarchar(100)` or `decimal(18,2)`.
    pub type_name: String,
    pub nullable: bool,
}

/// What a table has besides its columns' names and types.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableDetails {
    /// The server refused part of the lookup, so keys or indexes may be missing.
    pub incomplete: bool,
    pub column_extras: Vec<ColumnExtra>,
    pub constraints: Vec<TableConstraint>,
    pub indexes: Vec<TableIndex>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnExtra {
    pub column: String,
    pub clause: ColumnClause,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnClause {
    /// Follows the type: `default (0)`, `identity(1,1)`.
    AfterType(String),
    /// Stands in for the type and nullability: T-SQL's `as ([a] + [b])`.
    Computed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstraintKind {
    PrimaryKey,
    Unique,
    ForeignKey,
    Check,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableConstraint {
    pub name: String,
    pub kind: ConstraintKind,
    /// As written after `constraint <name>`: `primary key ([id])`.
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableIndex {
    pub name: String,
    /// `unique index (a, b)`, for a tree label.
    pub summary: String,
    /// The `create index` statement, without its `;`.
    pub statement: String,
}

/// `[name]`, with `]` doubled.
#[must_use]
pub fn quote_identifier(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// `N'text'`, with `'` doubled.
fn quote_literal(text: &str) -> String {
    format!("N'{}'", text.replace('\'', "''"))
}

// Catalog columns are compared as `type_desc`, not `type`: `type` is char(2),
// and Fabric's binary collation does not match 'U' against 'U '.
const OBJECT_TYPES: &str = "'USER_TABLE', 'VIEW', 'SQL_STORED_PROCEDURE', 'SQL_SCALAR_FUNCTION', \
     'SQL_INLINE_TABLE_VALUED_FUNCTION', 'SQL_TABLE_VALUED_FUNCTION'";

impl TdsConnection {
    /// User databases the login can open; on Fabric, the workspace's warehouses and lakehouses.
    pub async fn list_databases(&mut self) -> Result<Vec<String>, QueryError> {
        let result = self
            .query_one(
                "select name from sys.databases \
                 where name not in ('master', 'tempdb', 'model', 'msdb') \
                 and has_dbaccess(name) = 1 order by name",
            )
            .await?;
        Ok(result
            .rows
            .iter()
            .filter_map(|row| row.first().map(text))
            .collect())
    }

    pub async fn list_objects(&mut self, database: &str) -> Result<Vec<CatalogObject>, QueryError> {
        let database = quote_identifier(database);
        let sql = format!(
            "select s.name, o.name, o.type_desc \
             from {database}.sys.objects o \
             join {database}.sys.schemas s on s.schema_id = o.schema_id \
             where o.is_ms_shipped = 0 and o.type_desc in ({OBJECT_TYPES}) \
             order by s.name, o.name"
        );
        let result = self.query_one(&sql).await?;
        Ok(result
            .rows
            .iter()
            .filter_map(|row| {
                let [schema, name, type_desc] = row.as_slice() else {
                    return None;
                };
                Some(CatalogObject {
                    schema: text(schema),
                    name: text(name),
                    kind: ObjectKind::from_type_desc(&text(type_desc))?,
                })
            })
            .collect())
    }

    pub async fn list_columns(
        &mut self,
        database: &str,
        object: &CatalogObject,
    ) -> Result<Vec<ColumnInfo>, QueryError> {
        let quoted_database = quote_identifier(database);
        let full_name = format!("{quoted_database}.{}", object.qualified_name(Dialect::TSql));
        let sql = format!(
            "select c.name, t.name, c.max_length, c.precision, c.scale, c.is_nullable \
             from {quoted_database}.sys.columns c \
             join {quoted_database}.sys.types t on t.user_type_id = c.user_type_id \
             where c.object_id = object_id({}) order by c.column_id",
            quote_literal(&full_name)
        );
        let result = self.query_one(&sql).await?;
        Ok(result
            .rows
            .iter()
            .filter_map(|row| column_info(row))
            .collect())
    }

    /// Defaults, identity, keys, checks and indexes of a table. A part the
    /// server cannot list (a Fabric lakehouse has no index catalog) is left out.
    pub async fn table_details(
        &mut self,
        database: &str,
        object: &CatalogObject,
    ) -> Result<TableDetails, QueryError> {
        let sys = format!("{}.sys", quote_identifier(database));
        let table = object.qualified_name(Dialect::TSql);
        let object_id = format!(
            "object_id({})",
            quote_literal(&format!("{}.{table}", quote_identifier(database)))
        );
        // XML, spatial and hash indexes are left out: their statements need more than columns.
        let mut incomplete = false;
        let extras = self
            .optional_rows(
                &mut incomplete,
                &format!(
                    "select c.name, d.definition, convert(nvarchar(60), i.seed_value), \
                 convert(nvarchar(60), i.increment_value), cc.definition, cc.is_persisted \
                 from {sys}.columns c \
                 left join {sys}.default_constraints d \
                   on d.parent_object_id = c.object_id and d.parent_column_id = c.column_id \
                 left join {sys}.identity_columns i \
                   on i.object_id = c.object_id and i.column_id = c.column_id \
                 left join {sys}.computed_columns cc \
                   on cc.object_id = c.object_id and cc.column_id = c.column_id \
                 where c.object_id = {object_id} order by c.column_id"
                ),
            )
            .await?;
        let indexes = self
            .optional_rows(
                &mut incomplete,
                &format!(
                    "select i.name, i.is_primary_key, i.is_unique_constraint, i.is_unique, \
                 i.type_desc, i.filter_definition, c.name, ic.is_descending_key, \
                 ic.is_included_column \
                 from {sys}.indexes i \
                 join {sys}.index_columns ic \
                   on ic.object_id = i.object_id and ic.index_id = i.index_id \
                 join {sys}.columns c \
                   on c.object_id = ic.object_id and c.column_id = ic.column_id \
                 where i.object_id = {object_id} and i.type in (1, 2, 5, 6) \
                   and i.is_hypothetical = 0 \
                 order by i.index_id, ic.is_included_column, ic.key_ordinal, ic.index_column_id"
                ),
            )
            .await?;
        let foreign_keys = self
            .optional_rows(
                &mut incomplete,
                &format!(
                    "select fk.name, pc.name, rs.name, rt.name, rc.name, \
                 fk.delete_referential_action_desc, fk.update_referential_action_desc \
                 from {sys}.foreign_keys fk \
                 join {sys}.foreign_key_columns fkc on fkc.constraint_object_id = fk.object_id \
                 join {sys}.columns pc \
                   on pc.object_id = fkc.parent_object_id and pc.column_id = fkc.parent_column_id \
                 join {sys}.columns rc \
                   on rc.object_id = fkc.referenced_object_id \
                  and rc.column_id = fkc.referenced_column_id \
                 join {sys}.tables rt on rt.object_id = fk.referenced_object_id \
                 join {sys}.schemas rs on rs.schema_id = rt.schema_id \
                 where fk.parent_object_id = {object_id} \
                 order by fk.name, fkc.constraint_column_id"
                ),
            )
            .await?;
        let checks = self
            .optional_rows(
                &mut incomplete,
                &format!(
                    "select name, definition from {sys}.check_constraints \
                 where parent_object_id = {object_id} order by name"
                ),
            )
            .await?;

        let (mut constraints, indexes) = tsql_indexes(&table, &indexes);
        constraints.extend(tsql_foreign_keys(&foreign_keys));
        constraints.extend(checks.iter().filter_map(|row| {
            let [name, definition] = row.as_slice() else {
                return None;
            };
            Some(TableConstraint {
                name: text(name),
                kind: ConstraintKind::Check,
                definition: format!("check {}", text(definition)),
            })
        }));
        Ok(TableDetails {
            incomplete,
            column_extras: extras
                .iter()
                .filter_map(|row| tsql_column_extra(row))
                .collect(),
            constraints,
            indexes,
        })
    }

    /// The rows, or none and `refused` set when the server refuses the query; a lost connection still fails.
    async fn optional_rows(
        &mut self,
        refused: &mut bool,
        sql: &str,
    ) -> Result<Vec<Vec<Value>>, QueryError> {
        match self.query_one(sql).await {
            Ok(result) => Ok(result.rows),
            Err(QueryError::Server(_) | QueryError::ServerAt { .. }) => {
                *refused = true;
                Ok(Vec::new())
            }
            Err(
                error @ (QueryError::Auth(_)
                | QueryError::Connect(_)
                | QueryError::ConnectionLost(_)
                | QueryError::Cancelled),
            ) => Err(error),
        }
    }

    /// Source of a view, procedure or function; `None` for tables or encrypted modules.
    pub async fn object_definition(
        &mut self,
        database: &str,
        object: &CatalogObject,
    ) -> Result<Option<String>, QueryError> {
        let quoted_database = quote_identifier(database);
        let full_name = format!("{quoted_database}.{}", object.qualified_name(Dialect::TSql));
        let sql = format!(
            "select definition from {quoted_database}.sys.sql_modules \
             where object_id = object_id({})",
            quote_literal(&full_name)
        );
        let result = self.query_one(&sql).await?;
        Ok(result
            .rows
            .first()
            .and_then(|row| row.first())
            .filter(|value| !matches!(value, Value::Null))
            .map(text))
    }

    /// Every column of every table and view in `database`, in one query.
    pub async fn list_all_columns(
        &mut self,
        database: &str,
    ) -> Result<Vec<SchemaColumn>, QueryError> {
        let database = quote_identifier(database);
        let sql = format!(
            "select s.name, o.name, c.name, t.name, c.max_length, c.precision, c.scale, c.is_nullable \
             from {database}.sys.columns c \
             join {database}.sys.objects o on o.object_id = c.object_id \
             join {database}.sys.schemas s on s.schema_id = o.schema_id \
             join {database}.sys.types t on t.user_type_id = c.user_type_id \
             where o.is_ms_shipped = 0 and o.type_desc in ('USER_TABLE', 'VIEW') \
             order by s.name, o.name, c.column_id"
        );
        let result = self.query_one(&sql).await?;
        Ok(result
            .rows
            .iter()
            .filter_map(|row| {
                let (owner, rest) = row.split_at_checked(2)?;
                let [schema, table] = owner else {
                    return None;
                };
                Some(SchemaColumn {
                    schema: text(schema),
                    table: text(table),
                    column: column_info(rest)?,
                })
            })
            .collect())
    }

    async fn query_one(&mut self, sql: &str) -> Result<ResultSet, QueryError> {
        // Catalog reads feed completion, which must see every column.
        let output = self
            .execute_with_limit(sql, &CancelHandle::new(), usize::MAX)
            .await?;
        let first = output.result_sets.into_iter().next().unwrap_or_default();
        // Nothing else holds a catalog result, so this moves the rows out.
        Ok(Arc::try_unwrap(first).unwrap_or_else(|shared| (*shared).clone()))
    }
}

fn text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Decimal(_)
        | Value::Text(_)
        | Value::Bytes(_)
        | Value::Date(_)
        | Value::Time(_)
        | Value::Timestamp(_) => value.to_string(),
    }
}

fn int(value: &Value) -> i64 {
    match value {
        Value::Int(number) => *number,
        Value::Bool(flag) => i64::from(*flag),
        Value::Null
        | Value::Float(_)
        | Value::Decimal(_)
        | Value::Text(_)
        | Value::Bytes(_)
        | Value::Date(_)
        | Value::Time(_)
        | Value::Timestamp(_) => 0,
    }
}

fn column_info(row: &[Value]) -> Option<ColumnInfo> {
    let [name, type_name, max_length, precision, scale, nullable] = row else {
        return None;
    };
    Some(ColumnInfo {
        name: text(name),
        type_name: declared_type(
            &text(type_name),
            int(max_length),
            int(precision),
            int(scale),
        ),
        nullable: int(nullable) != 0,
    })
}

/// Type as written in DDL from `sys.columns` sizes. `max_length` is in bytes
/// and `-1` for `max`; `nchar`/`nvarchar` use two bytes per character.
fn declared_type(type_name: &str, max_length: i64, precision: i64, scale: i64) -> String {
    let length = |bytes_per_char: i64| {
        if max_length == -1 {
            "max".to_owned()
        } else {
            (max_length / bytes_per_char).to_string()
        }
    };
    match type_name {
        "char" | "varchar" | "binary" | "varbinary" => format!("{type_name}({})", length(1)),
        "nchar" | "nvarchar" => format!("{type_name}({})", length(2)),
        "decimal" | "numeric" => format!("{type_name}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" => format!("{type_name}({scale})"),
        _ => type_name.to_owned(),
    }
}

fn tsql_column_extra(row: &[Value]) -> Option<ColumnExtra> {
    let [column, default, seed, increment, computed, persisted] = row else {
        return None;
    };
    let (default, seed, increment, computed) =
        (text(default), text(seed), text(increment), text(computed));
    let clause = if !computed.is_empty() {
        let persisted = if int(persisted) != 0 {
            " persisted"
        } else {
            ""
        };
        ColumnClause::Computed(format!("as {computed}{persisted}"))
    } else if !seed.is_empty() {
        ColumnClause::AfterType(format!("identity({seed},{increment})"))
    } else if !default.is_empty() {
        ColumnClause::AfterType(format!("default {default}"))
    } else {
        return None;
    };
    Some(ColumnExtra {
        column: text(column),
        clause,
    })
}

struct IndexColumns {
    name: String,
    primary_key: bool,
    unique_constraint: bool,
    unique: bool,
    type_desc: String,
    filter: String,
    /// Quoted for the statement, and bare for the label.
    keys: Vec<(String, String)>,
    included: Vec<String>,
}

/// Groups one `sys.indexes` row per index column by index.
fn index_columns(rows: &[Vec<Value>]) -> Vec<IndexColumns> {
    let mut found: Vec<IndexColumns> = Vec::new();
    for row in rows {
        let [
            name,
            primary_key,
            unique_constraint,
            unique,
            type_desc,
            filter,
            column,
            descending,
            included,
        ] = row.as_slice()
        else {
            continue;
        };
        let name = text(name);
        if found.last().is_none_or(|index| index.name != name) {
            found.push(IndexColumns {
                name,
                primary_key: int(primary_key) != 0,
                unique_constraint: int(unique_constraint) != 0,
                unique: int(unique) != 0,
                type_desc: text(type_desc),
                filter: text(filter),
                keys: Vec::new(),
                included: Vec::new(),
            });
        }
        let Some(index) = found.last_mut() else {
            continue;
        };
        let column = text(column);
        if int(included) != 0 {
            index.included.push(quote_identifier(&column));
        } else {
            let order = if int(descending) != 0 { " desc" } else { "" };
            index.keys.push((
                format!("{}{order}", quote_identifier(&column)),
                format!("{column}{order}"),
            ));
        }
    }
    found
}

/// Key constraints and `create index` statements on `table`, from one
/// `sys.indexes` row per index column.
fn tsql_indexes(table: &str, rows: &[Vec<Value>]) -> (Vec<TableConstraint>, Vec<TableIndex>) {
    let mut constraints = Vec::new();
    let mut indexes = Vec::new();
    for index in index_columns(rows) {
        let quoted: Vec<&str> = index
            .keys
            .iter()
            .map(|(quoted, _)| quoted.as_str())
            .collect();
        let bare: Vec<&str> = index.keys.iter().map(|(_, bare)| bare.as_str()).collect();
        let clustered = if index.type_desc.starts_with("CLUSTERED") {
            "clustered"
        } else {
            "nonclustered"
        };
        let name = quote_identifier(&index.name);
        if index.primary_key || index.unique_constraint {
            let (kind, word) = if index.primary_key {
                (ConstraintKind::PrimaryKey, "primary key")
            } else {
                (ConstraintKind::Unique, "unique")
            };
            constraints.push(TableConstraint {
                name: index.name,
                kind,
                definition: format!("{word} {clustered} ({})", quoted.join(", ")),
            });
        } else if index.type_desc.contains("COLUMNSTORE") {
            // A columnstore index has no key; its columns come back as included.
            let columns = if clustered == "clustered" || index.included.is_empty() {
                String::new()
            } else {
                format!(" ({})", index.included.join(", "))
            };
            let filter = if index.filter.is_empty() {
                String::new()
            } else {
                format!(" where {}", index.filter)
            };
            indexes.push(TableIndex {
                summary: format!("{clustered} columnstore index"),
                statement: format!(
                    "create {clustered} columnstore index {name} on {table}{columns}{filter}"
                ),
                name: index.name,
            });
        } else {
            let unique = if index.unique { "unique " } else { "" };
            let include = if index.included.is_empty() {
                String::new()
            } else {
                format!(" include ({})", index.included.join(", "))
            };
            let filter = if index.filter.is_empty() {
                String::new()
            } else {
                format!(" where {}", index.filter)
            };
            let statement = format!(
                "create {unique}{clustered} index {name} on {table} ({}){include}{filter}",
                quoted.join(", ")
            );
            indexes.push(TableIndex {
                summary: format!("{unique}index ({})", bare.join(", ")),
                statement,
                name: index.name,
            });
        }
    }
    (constraints, indexes)
}

/// One row per foreign key column, grouped into one constraint per key.
fn tsql_foreign_keys(rows: &[Vec<Value>]) -> Vec<TableConstraint> {
    struct ForeignKey {
        name: String,
        columns: Vec<String>,
        references: String,
        referenced_columns: Vec<String>,
        actions: String,
    }
    let action = |event: &str, description: &Value| match text(description).as_str() {
        "CASCADE" => format!(" on {event} cascade"),
        "SET_NULL" => format!(" on {event} set null"),
        "SET_DEFAULT" => format!(" on {event} set default"),
        _ => String::new(),
    };
    let mut found: Vec<ForeignKey> = Vec::new();
    for row in rows {
        let [
            name,
            column,
            schema,
            table,
            referenced,
            on_delete,
            on_update,
        ] = row.as_slice()
        else {
            continue;
        };
        let name = text(name);
        if found.last().is_none_or(|key| key.name != name) {
            found.push(ForeignKey {
                name,
                columns: Vec::new(),
                references: format!(
                    "{}.{}",
                    quote_identifier(&text(schema)),
                    quote_identifier(&text(table))
                ),
                referenced_columns: Vec::new(),
                actions: format!(
                    "{}{}",
                    action("delete", on_delete),
                    action("update", on_update)
                ),
            });
        }
        if let Some(key) = found.last_mut() {
            key.columns.push(quote_identifier(&text(column)));
            key.referenced_columns
                .push(quote_identifier(&text(referenced)));
        }
    }
    found
        .into_iter()
        .map(|key| TableConstraint {
            definition: format!(
                "foreign key ({}) references {} ({}){}",
                key.columns.join(", "),
                key.references,
                key.referenced_columns.join(", "),
                key.actions
            ),
            name: key.name,
            kind: ConstraintKind::ForeignKey,
        })
        .collect()
}

/// `select top N [a], [b] from [schema].[name];` (or `limit N` on PostgreSQL),
/// with `*` when columns are unknown.
#[must_use]
pub fn select_script(
    dialect: Dialect,
    object: &CatalogObject,
    columns: Option<&[ColumnInfo]>,
    top: Option<usize>,
) -> String {
    let column_list = match columns {
        Some(columns) if !columns.is_empty() => columns
            .iter()
            .map(|column| format!("    {}", dialect.quote_identifier(&column.name)))
            .collect::<Vec<_>>()
            .join(",\n"),
        _ => "    *".to_owned(),
    };
    let name = object.qualified_name(dialect);
    match (dialect, top) {
        (Dialect::TSql, Some(rows)) => format!("select top {rows}\n{column_list}\nfrom {name};\n"),
        (Dialect::Postgres | Dialect::Sqlite | Dialect::Snowflake, Some(rows)) => {
            format!("select\n{column_list}\nfrom {name}\nlimit {rows};\n")
        }
        (Dialect::TSql | Dialect::Postgres | Dialect::Sqlite | Dialect::Snowflake, None) => {
            format!("select\n{column_list}\nfrom {name};\n")
        }
    }
}

/// `create table` with defaults, identity, keys and checks, then one
/// `create index` per index.
#[must_use]
pub fn create_table_script(
    dialect: Dialect,
    object: &CatalogObject,
    columns: &[ColumnInfo],
    details: &TableDetails,
) -> String {
    let mut lines: Vec<String> = columns
        .iter()
        .map(|column| {
            let name = dialect.quote_identifier(&column.name);
            let mut parts = vec![name.as_str(), column.type_name.as_str()];
            for extra in &details.column_extras {
                match &extra.clause {
                    ColumnClause::Computed(expression) if extra.column == column.name => {
                        return format!("    {name} {expression}");
                    }
                    ColumnClause::AfterType(clause) if extra.column == column.name => {
                        parts.push(clause);
                    }
                    ColumnClause::Computed(_) | ColumnClause::AfterType(_) => {}
                }
            }
            parts.push(if column.nullable { "null" } else { "not null" });
            format!("    {}", parts.join(" "))
        })
        .collect();
    lines.extend(details.constraints.iter().map(|constraint| {
        format!(
            "    constraint {} {}",
            dialect.quote_identifier(&constraint.name),
            constraint.definition
        )
    }));
    let warning = if details.incomplete {
        "-- The server refused part of the lookup: keys, defaults or indexes may be missing.\n"
    } else {
        ""
    };
    let mut script = format!(
        "{warning}create table {} (\n{}\n);\n",
        object.qualified_name(dialect),
        lines.join(",\n")
    );
    for index in &details.indexes {
        script.push('\n');
        script.push_str(&index.statement);
        script.push_str(";\n");
    }
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> CatalogObject {
        CatalogObject {
            schema: "dbo".to_owned(),
            name: "odd]name".to_owned(),
            kind: ObjectKind::Table,
        }
    }

    fn columns() -> Vec<ColumnInfo> {
        vec![
            ColumnInfo {
                name: "id".to_owned(),
                type_name: "int".to_owned(),
                nullable: false,
            },
            ColumnInfo {
                name: "label".to_owned(),
                type_name: "nvarchar(100)".to_owned(),
                nullable: true,
            },
        ]
    }

    #[test]
    fn quotes_identifiers_and_literals() {
        assert_eq!(quote_identifier("a]b"), "[a]]b]");
        assert_eq!(quote_literal("it's"), "N'it''s'");
        assert_eq!(table().qualified_name(Dialect::TSql), "[dbo].[odd]]name]");
        assert_eq!(Dialect::TSql.name_in_sql("Orders"), "Orders");
        assert_eq!(Dialect::TSql.name_in_sql("order lines"), "[order lines]");
        assert_eq!(Dialect::TSql.name_in_sql("Order"), "[Order]");
        assert_eq!(Dialect::Postgres.name_in_sql("orders"), "orders");
        assert_eq!(Dialect::Postgres.name_in_sql("Orders"), r#""Orders""#);
        assert_eq!(Dialect::Snowflake.name_in_sql("ORDERS"), "ORDERS");
        assert_eq!(Dialect::Snowflake.name_in_sql("orders"), r#""orders""#);
        assert_eq!(Dialect::Postgres.quote_identifier(r#"a"b"#), r#""a""b""#);
    }

    #[test]
    fn formats_declared_types() {
        assert_eq!(declared_type("nvarchar", 200, 0, 0), "nvarchar(100)");
        assert_eq!(declared_type("varchar", -1, 0, 0), "varchar(max)");
        assert_eq!(declared_type("decimal", 9, 18, 2), "decimal(18,2)");
        assert_eq!(declared_type("datetime2", 8, 27, 6), "datetime2(6)");
        assert_eq!(declared_type("int", 4, 10, 0), "int");
    }

    #[test]
    fn maps_type_descriptions() {
        assert_eq!(
            ObjectKind::from_type_desc("USER_TABLE"),
            Some(ObjectKind::Table)
        );
        assert_eq!(
            ObjectKind::from_type_desc("SQL_INLINE_TABLE_VALUED_FUNCTION"),
            Some(ObjectKind::Function)
        );
        assert_eq!(ObjectKind::from_type_desc("SERVICE_QUEUE"), None);
    }

    #[test]
    fn scripts_select_and_create() {
        assert_eq!(
            select_script(Dialect::TSql, &table(), Some(&columns()), Some(1000)),
            "select top 1000\n    [id],\n    [label]\nfrom [dbo].[odd]]name];\n"
        );
        assert_eq!(
            select_script(Dialect::TSql, &table(), None, None),
            "select\n    *\nfrom [dbo].[odd]]name];\n"
        );
        assert_eq!(
            create_table_script(
                Dialect::TSql,
                &table(),
                &columns(),
                &TableDetails::default()
            ),
            "create table [dbo].[odd]]name] (\n    [id] int not null,\n    [label] nvarchar(100) null\n);\n"
        );
        assert_eq!(
            select_script(Dialect::Postgres, &table(), Some(&columns()), Some(10)),
            "select\n    \"id\",\n    \"label\"\nfrom \"dbo\".\"odd]name\"\nlimit 10;\n"
        );
    }

    fn value_row(values: &[&str]) -> Vec<Value> {
        values
            .iter()
            .map(|value| match *value {
                "" => Value::Null,
                "0" => Value::Bool(false),
                "1" => Value::Bool(true),
                other => Value::Text(other.to_owned()),
            })
            .collect()
    }

    #[test]
    fn scripts_a_table_with_its_keys_and_indexes() {
        let index_rows = [
            value_row(&["pk", "1", "0", "1", "CLUSTERED", "", "id", "0", "0"]),
            value_row(&["uq", "0", "1", "1", "NONCLUSTERED", "", "label", "0", "0"]),
            value_row(&[
                "ix",
                "0",
                "0",
                "0",
                "NONCLUSTERED",
                "([id]>(0))",
                "label",
                "1",
                "0",
            ]),
            value_row(&[
                "ix",
                "0",
                "0",
                "0",
                "NONCLUSTERED",
                "([id]>(0))",
                "id",
                "0",
                "1",
            ]),
        ];
        let (mut constraints, indexes) = tsql_indexes("[dbo].[t]", &index_rows);
        assert_eq!(indexes[0].summary, "index (label desc)");
        constraints.extend(tsql_foreign_keys(&[
            value_row(&["fk", "id", "dbo", "parent", "a", "CASCADE", "NO_ACTION"]),
            value_row(&["fk", "label", "dbo", "parent", "b", "CASCADE", "NO_ACTION"]),
        ]));
        let details = TableDetails {
            incomplete: false,
            column_extras: [
                value_row(&["id", "", "1", "1", "", ""]),
                value_row(&["label", "(N'x')", "", "", "", ""]),
                value_row(&["plain", "", "", "", "", ""]),
            ]
            .iter()
            .filter_map(|row| tsql_column_extra(row))
            .collect(),
            constraints,
            indexes,
        };
        assert_eq!(
            create_table_script(Dialect::TSql, &table(), &columns(), &details),
            "create table [dbo].[odd]]name] (\n    \
             [id] int identity(1,1) not null,\n    \
             [label] nvarchar(100) default (N'x') null,\n    \
             constraint [pk] primary key clustered ([id]),\n    \
             constraint [uq] unique nonclustered ([label]),\n    \
             constraint [fk] foreign key ([id], [label]) references [dbo].[parent] ([a], [b]) on delete cascade\n\
             );\n\n\
             create nonclustered index [ix] on [dbo].[t] ([label] desc) include ([id]) where ([id]>(0));\n"
        );
        let computed = tsql_column_extra(&value_row(&["id", "", "", "", "([a]+[b])", "1"]));
        assert_eq!(
            computed.map(|extra| extra.clause),
            Some(ColumnClause::Computed("as ([a]+[b]) persisted".to_owned()))
        );
    }
}
