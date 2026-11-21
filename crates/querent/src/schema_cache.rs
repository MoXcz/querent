//! Database names per connection, and tables, views and columns per database,
//! for the tab's database picker, completion and hover.
//!
//! Loaded in the background on first use with two catalog queries (objects,
//! then every column at once) on a connection of its own, so typing never
//! waits on a tab's query or the explorer.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{App, BorrowAppContext as _, Global, SharedString};
use sql_db::{CatalogObject, ColumnInfo, Connection, Dialect, ObjectKind, SchemaColumn};
use sql_text::completion::TableRef;
use tokio::sync::Mutex;

use crate::connections::{self, ConnectionStore};
use crate::tokio_bridge;

/// A loaded schema; names compare without case, as SQL Server and most
/// unquoted SQL do.
#[derive(Debug, Default)]
pub struct Schema {
    pub dialect: Option<Dialect>,
    /// Tables and views.
    pub objects: Vec<CatalogObject>,
    columns: HashMap<(String, String), Vec<ColumnInfo>>,
}

impl Schema {
    pub fn new(dialect: Dialect, objects: Vec<CatalogObject>, columns: Vec<SchemaColumn>) -> Self {
        let mut by_table: HashMap<(String, String), Vec<ColumnInfo>> = HashMap::new();
        for entry in columns {
            by_table
                .entry(key(&entry.schema, &entry.table))
                .or_default()
                .push(entry.column);
        }
        Self {
            dialect: Some(dialect),
            objects: objects
                .into_iter()
                .filter(|object| matches!(object.kind, ObjectKind::Table | ObjectKind::View))
                .collect(),
            columns: by_table,
        }
    }

    /// The table or view `table` refers to, with its columns.
    pub fn resolve(&self, table: &TableRef) -> Option<(&CatalogObject, &[ColumnInfo])> {
        let object = self.objects.iter().find(|object| {
            object.name.eq_ignore_ascii_case(&table.name)
                && table
                    .schema
                    .as_deref()
                    .is_none_or(|schema| object.schema.eq_ignore_ascii_case(schema))
        })?;
        let columns = self
            .columns
            .get(&key(&object.schema, &object.name))
            .map_or(&[][..], Vec::as_slice);
        Some((object, columns))
    }

    /// Schema names, each once.
    pub fn schemas(&self) -> Vec<&str> {
        let mut schemas: Vec<&str> = self
            .objects
            .iter()
            .map(|object| object.schema.as_str())
            .collect();
        schemas.dedup();
        schemas
    }
}

fn key(schema: &str, table: &str) -> (String, String) {
    (schema.to_lowercase(), table.to_lowercase())
}

/// A failed load is retried after this long, not on every keystroke.
const RETRY_AFTER: Duration = Duration::from_secs(30);

enum Entry {
    /// The number tells this load's result from an older, superseded one.
    Loading(u64),
    Loaded(Arc<Schema>),
    Failed(Instant),
}

type CatalogConnection = Arc<Mutex<Option<Connection>>>;

enum DatabaseList {
    Loading(u64),
    Loaded(Arc<Vec<SharedString>>),
    Failed(Instant),
}

#[derive(Default)]
pub struct SchemaCache {
    entries: HashMap<(SharedString, String), Entry>,
    databases: HashMap<SharedString, DatabaseList>,
    next_load: u64,
    connections: HashMap<SharedString, CatalogConnection>,
}

impl Global for SchemaCache {}

pub fn init(cx: &mut App) {
    cx.set_global(SchemaCache::default());
    // A saved connection may now point elsewhere.
    cx.observe_global::<ConnectionStore>(|cx| {
        cx.update_global::<SchemaCache, _>(|cache, _| {
            cache.entries.clear();
            cache.databases.clear();
            cache.connections.clear();
        });
    })
    .detach();
}

/// The database's schema when loaded; otherwise starts loading it and returns `None`.
pub fn get(cx: &mut App, connection: &SharedString, database: &str) -> Option<Arc<Schema>> {
    let entry_key = (connection.clone(), database.to_owned());
    match cx.global::<SchemaCache>().entries.get(&entry_key) {
        Some(Entry::Loaded(schema)) => return Some(schema.clone()),
        Some(Entry::Loading(_)) => return None,
        Some(Entry::Failed(at)) if at.elapsed() < RETRY_AFTER => return None,
        Some(Entry::Failed(_)) | None => {}
    }
    load(cx, entry_key);
    None
}

/// Drops loaded schemas, of one database or of every database of the
/// connection, so the next use reloads them.
pub fn invalidate(cx: &mut App, connection: &SharedString, database: Option<&str>) {
    cx.update_global::<SchemaCache, _>(|cache, _| {
        cache.entries.retain(|(owner, name), _| {
            owner != connection || database.is_some_and(|database| database != name)
        });
        if database.is_none() {
            cache.databases.remove(connection);
        }
    });
}

/// The connection's database names, when loaded.
pub fn databases(cx: &App, connection: &SharedString) -> Option<Arc<Vec<SharedString>>> {
    match cx.global::<SchemaCache>().databases.get(connection)? {
        DatabaseList::Loaded(names) => Some(names.clone()),
        DatabaseList::Loading(_) | DatabaseList::Failed(_) => None,
    }
}

/// Stores names the explorer loaded, so the picker need not ask again.
pub fn set_databases(cx: &mut App, connection: &SharedString, names: Arc<Vec<SharedString>>) {
    cx.update_global::<SchemaCache, _>(|cache, _| {
        cache
            .databases
            .insert(connection.clone(), DatabaseList::Loaded(names));
    });
}

/// Starts loading the connection's database names unless they are loaded or loading.
pub fn load_databases(cx: &mut App, connection: &SharedString) {
    match cx.global::<SchemaCache>().databases.get(connection) {
        Some(DatabaseList::Loaded(_) | DatabaseList::Loading(_)) => return,
        Some(DatabaseList::Failed(at)) if at.elapsed() < RETRY_AFTER => return,
        Some(DatabaseList::Failed(_)) | None => {}
    }
    let Some(config) = connections::find(cx, connection).and_then(|saved| saved.config().ok())
    else {
        return;
    };
    let connection = connection.clone();
    let (load_id, catalog) = cx.update_global::<SchemaCache, _>(|cache, _| {
        cache.next_load += 1;
        let load_id = cache.next_load;
        cache
            .databases
            .insert(connection.clone(), DatabaseList::Loading(load_id));
        (load_id, catalog_connection(cache, &connection))
    });
    let task = tokio_bridge::spawn(cx, async move {
        with_catalog(&catalog, config, |connection| {
            Box::pin(connection.list_databases())
        })
        .await
    });
    cx.spawn(async move |cx| {
        let list = match task.await {
            Ok(Ok(names)) => {
                DatabaseList::Loaded(Arc::new(names.into_iter().map(SharedString::from).collect()))
            }
            Ok(Err(error)) => {
                tracing::debug!(%error, "database list failed");
                DatabaseList::Failed(Instant::now())
            }
            Err(error) => {
                tracing::debug!(%error, "database list task failed");
                DatabaseList::Failed(Instant::now())
            }
        };
        cx.update(|cx| {
            cx.update_global::<SchemaCache, _>(|cache, _| {
                if matches!(cache.databases.get(&connection), Some(DatabaseList::Loading(id)) if *id == load_id) {
                    cache.databases.insert(connection, list);
                }
            });
        });
    })
    .detach();
}

fn catalog_connection(cache: &mut SchemaCache, connection: &SharedString) -> CatalogConnection {
    cache
        .connections
        .entry(connection.clone())
        .or_insert_with(|| Arc::new(Mutex::new(None)))
        .clone()
}

type CatalogWork<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, sql_db::QueryError>> + Send + 'a>>;

/// Runs `work` on the cache's own connection, opening it first, and drops it when lost.
async fn with_catalog<T>(
    catalog: &CatalogConnection,
    config: sql_db::ConnectionConfig,
    work: impl for<'a> FnOnce(&'a mut Connection) -> CatalogWork<'a, T>,
) -> Result<T, sql_db::QueryError> {
    let mut guard = catalog.lock().await;
    if guard.is_none() {
        *guard = Some(Connection::connect(config).await?);
    }
    let Some(connection) = guard.as_mut() else {
        return Err(sql_db::QueryError::ConnectionLost(
            "schema connection missing".to_owned(),
        ));
    };
    let result = work(connection).await;
    if matches!(result, Err(sql_db::QueryError::ConnectionLost(_))) {
        *guard = None;
    }
    result
}

fn load(cx: &mut App, entry_key: (SharedString, String)) {
    let (connection_name, database) = entry_key.clone();
    let Some(saved) = connections::find(cx, &connection_name) else {
        return;
    };
    let dialect = saved.dialect();
    let Ok(config) = saved.config() else {
        return;
    };
    let (load_id, catalog) = cx.update_global::<SchemaCache, _>(|cache, _| {
        cache.next_load += 1;
        let load_id = cache.next_load;
        cache
            .entries
            .insert(entry_key.clone(), Entry::Loading(load_id));
        (load_id, catalog_connection(cache, &connection_name))
    });
    let task = tokio_bridge::spawn(cx, async move {
        with_catalog(&catalog, config, |connection| {
            Box::pin(async move {
                let objects = connection.list_objects(&database).await?;
                let columns = connection.list_all_columns(&database).await?;
                Ok((objects, columns))
            })
        })
        .await
    });
    cx.spawn(async move |cx| {
        let entry = match task.await {
            Ok(Ok((objects, columns))) => {
                tracing::debug!(objects = objects.len(), "schema loaded");
                Entry::Loaded(Arc::new(Schema::new(dialect, objects, columns)))
            }
            Ok(Err(error)) => {
                tracing::debug!(%error, "schema load failed");
                Entry::Failed(Instant::now())
            }
            Err(error) => {
                tracing::debug!(%error, "schema task failed");
                Entry::Failed(Instant::now())
            }
        };
        cx.update(|cx| {
            cx.update_global::<SchemaCache, _>(|cache, _| {
                // After a reset or refresh, a newer load (or none) owns the entry.
                if matches!(cache.entries.get(&entry_key), Some(Entry::Loading(id)) if *id == load_id) {
                    cache.entries.insert(entry_key, entry);
                }
            });
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_tables_without_case() {
        let mut columns = HashMap::new();
        columns.insert(
            key("dbo", "Customers"),
            vec![ColumnInfo {
                name: "Id".to_owned(),
                type_name: "int".to_owned(),
                nullable: false,
            }],
        );
        let schema = Schema {
            dialect: Some(Dialect::TSql),
            objects: vec![CatalogObject {
                schema: "dbo".to_owned(),
                name: "Customers".to_owned(),
                kind: ObjectKind::Table,
            }],
            columns,
        };
        let table = TableRef {
            schema: None,
            name: "customers".to_owned(),
            alias: Some("c".to_owned()),
        };
        let (object, columns) = schema.resolve(&table).unwrap();
        assert_eq!(object.name, "Customers");
        assert_eq!(columns.len(), 1);
        let other_schema = TableRef {
            schema: Some("sales".to_owned()),
            ..table
        };
        assert!(schema.resolve(&other_schema).is_none());
    }
}
