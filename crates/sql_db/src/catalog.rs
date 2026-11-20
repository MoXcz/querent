//! Database objects for the object explorer, and scripts generated from them.
//!
//! SQL Server queries name the database explicitly (`[db].sys.objects`), so one
//! connection can browse every database on the server or Fabric workspace.

use crate::{CancelHandle, QueryError, ResultSet, TdsConnection, Value};

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

    async fn query_one(&mut self, sql: &str) -> Result<ResultSet, QueryError> {
        let output = self.execute(sql, &CancelHandle::new()).await?;
        Ok(output.result_sets.into_iter().next().unwrap_or_default())
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

#[must_use]
pub fn create_table_script(
    dialect: Dialect,
    object: &CatalogObject,
    columns: &[ColumnInfo],
) -> String {
    let lines: Vec<String> = columns
        .iter()
        .map(|column| {
            let null = if column.nullable { "null" } else { "not null" };
            format!(
                "    {} {} {null}",
                dialect.quote_identifier(&column.name),
                column.type_name
            )
        })
        .collect();
    format!(
        "create table {} (\n{}\n);\n",
        object.qualified_name(dialect),
        lines.join(",\n")
    )
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
            create_table_script(Dialect::TSql, &table(), &columns()),
            "create table [dbo].[odd]]name] (\n    [id] int not null,\n    [label] nvarchar(100) null\n);\n"
        );
        assert_eq!(
            select_script(Dialect::Postgres, &table(), Some(&columns()), Some(10)),
            "select\n    \"id\",\n    \"label\"\nfrom \"dbo\".\"odd]name\"\nlimit 10;\n"
        );
    }
}
