//! Sidebar tree of saved connections and their databases, schemas, objects and
//! columns, loaded lazily on expand, like SSMS's Object Explorer.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, EventEmitter,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px, uniform_list,
};
use sql_db::{
    CatalogObject, ColumnInfo, Connection, ConnectionConfig, Dialect, ObjectKind, QueryError,
};
use tokio::sync::Mutex;

use crate::connections::{self, ConnectionStore, SavedConnection};
use crate::schema_cache;
use crate::settings;
use crate::tokio_bridge;

const ROW_INDENT: f32 = 12.;
const SELECT_TOP_ROWS: usize = 1000;

pub enum ExplorerEvent {
    UseConnection {
        connection: SharedString,
        database: Option<SharedString>,
    },
    OpenQuery {
        connection: SharedString,
        database: SharedString,
        sql: String,
        run: bool,
    },
    InsertText(String),
    AddConnection,
    EditConnection(SavedConnection),
    /// Ask before deleting: the connection's stored secret goes with it.
    DeleteConnection(SharedString),
}

impl EventEmitter<ExplorerEvent> for ObjectExplorer {}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ObjectKey {
    connection: SharedString,
    database: SharedString,
    schema: SharedString,
    name: SharedString,
    kind: ObjectKind,
}

impl ObjectKey {
    fn catalog_object(&self) -> CatalogObject {
        CatalogObject {
            schema: self.schema.to_string(),
            name: self.name.to_string(),
            kind: self.kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum NodeId {
    Connection(SharedString),
    Database(SharedString, SharedString),
    Group(SharedString, SharedString, ObjectKind),
    Schema(SharedString, SharedString, ObjectKind, SharedString),
    Object(ObjectKey),
}

enum Load<T> {
    Loading,
    Loaded(T),
    Failed(SharedString),
}

enum RowContent {
    Node(NodeId),
    /// The connection name picks how a double-click quotes the column.
    Column(SharedString, ColumnInfo),
    Loading,
    Error,
    Empty,
}

struct Row {
    content: RowContent,
    depth: usize,
    label: SharedString,
    expanded: Option<bool>,
}

type MenuAction = Box<dyn Fn(&mut ObjectExplorer, &mut Context<ObjectExplorer>)>;

type CatalogFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, QueryError>> + Send + 'a>>;

/// One catalog connection per saved connection, connected on first use and
/// separate from the tabs' connections so browsing never waits on a query.
type CatalogConnection = Arc<Mutex<Option<Connection>>>;

pub struct ObjectExplorer {
    filter_input: Entity<InputState>,
    expanded: HashSet<NodeId>,
    selected: Option<NodeId>,
    databases: HashMap<SharedString, Load<Arc<Vec<SharedString>>>>,
    objects: HashMap<(SharedString, SharedString), Load<Arc<Vec<CatalogObject>>>>,
    columns: HashMap<ObjectKey, Load<Arc<Vec<ColumnInfo>>>>,
    catalog_connections: HashMap<SharedString, CatalogConnection>,
    /// Bumped when the connections change, so a load that started before is ignored.
    generation: u64,
    rows: Vec<Row>,
    /// Bumped when what the tree shows changes; the rows are rebuilt only then.
    tree_version: u64,
    /// The filter, grouping and tree version `rows` was built from.
    rows_built_for: Option<(String, bool, u64)>,
    _subscriptions: Vec<Subscription>,
}

impl ObjectExplorer {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter_input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter tables"));
        let subscriptions = vec![
            cx.subscribe(&filter_input, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.observe_global::<ConnectionStore>(Self::reset),
        ];
        Self {
            filter_input,
            expanded: HashSet::new(),
            selected: None,
            databases: HashMap::new(),
            objects: HashMap::new(),
            columns: HashMap::new(),
            catalog_connections: HashMap::new(),
            generation: 0,
            rows: Vec::new(),
            tree_version: 0,
            rows_built_for: None,
            _subscriptions: subscriptions,
        }
    }

    /// Saved connections changed: drop everything loaded, since a connection
    /// string may now point elsewhere.
    fn reset(&mut self, cx: &mut Context<Self>) {
        self.expanded.clear();
        self.selected = None;
        self.databases.clear();
        self.objects.clear();
        self.columns.clear();
        self.catalog_connections.clear();
        self.generation += 1;
        self.tree_version += 1;
        cx.notify();
    }

    fn catalog_connection(&mut self, name: &SharedString) -> CatalogConnection {
        self.catalog_connections
            .entry(name.clone())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    }

    fn config_for(name: &str, cx: &Context<Self>) -> Result<ConnectionConfig, SharedString> {
        let saved = connections::find(cx, name)
            .ok_or_else(|| SharedString::from(format!("Connection {name} no longer exists.")))?;
        saved
            .config()
            .map_err(|error| SharedString::from(error.to_string()))
    }

    /// T-SQL when the connection is gone, so a stale menu still produces something.
    fn dialect(connection: &str, cx: &App) -> Dialect {
        connections::find(cx, connection).map_or(Dialect::TSql, SavedConnection::dialect)
    }

    fn toggle(&mut self, id: &NodeId, cx: &mut Context<Self>) {
        self.tree_version += 1;
        if self.expanded.remove(id) {
            cx.notify();
            return;
        }
        self.expanded.insert(id.clone());
        tracing::debug!(node = ?id, "explorer node expanded");
        self.ensure_loaded(id, cx);
        cx.notify();
    }

    fn ensure_loaded(&mut self, id: &NodeId, cx: &mut Context<Self>) {
        match id {
            NodeId::Connection(connection) => {
                if !self.databases.contains_key(connection) {
                    self.load_databases(connection, cx);
                }
            }
            NodeId::Database(connection, database) => {
                let key = (connection.clone(), database.clone());
                if !self.objects.contains_key(&key) {
                    self.load_objects(key, cx);
                }
            }
            NodeId::Object(key) => {
                if matches!(key.kind, ObjectKind::Table | ObjectKind::View)
                    && !self.columns.contains_key(key)
                {
                    self.load_columns(key.clone(), cx);
                }
            }
            NodeId::Group(..) | NodeId::Schema(..) => {}
        }
    }

    /// Runs `work` on the connection's catalog connection, connecting first if needed.
    fn run_catalog<T, F>(
        &mut self,
        connection: &SharedString,
        cx: &mut Context<Self>,
        work: F,
        done: impl FnOnce(&mut Self, Result<T, SharedString>, &mut Context<Self>) + 'static,
    ) where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut Connection) -> CatalogFuture<'a, T> + Send + 'static,
    {
        let config = match Self::config_for(connection, cx) {
            Ok(config) => config,
            Err(message) => {
                done(self, Err(message), cx);
                self.tree_version += 1;
                return;
            }
        };
        let catalog = self.catalog_connection(connection);
        let connection_name = connection.clone();
        let generation = self.generation;
        let task = tokio_bridge::spawn(cx, async move {
            let started = std::time::Instant::now();
            tracing::debug!(connection = %connection_name, "catalog query queued");
            let mut guard = catalog.lock().await;
            tracing::debug!(connection = %connection_name, waited_ms = started.elapsed().as_millis(), "catalog lock acquired");
            if guard.is_none() {
                *guard = Some(Connection::connect(config).await?);
                tracing::debug!(connection = %connection_name, elapsed_ms = started.elapsed().as_millis(), "catalog connected");
            }
            let Some(tds) = guard.as_mut() else {
                return Err(QueryError::ConnectionLost(
                    "catalog connection missing".to_owned(),
                ));
            };
            let result = work(tds).await;
            tracing::debug!(connection = %connection_name, elapsed_ms = started.elapsed().as_millis(), ok = result.is_ok(), "catalog query finished");
            if matches!(result, Err(QueryError::ConnectionLost(_))) {
                *guard = None;
            }
            result
        });
        cx.spawn(async move |this, cx| {
            let result = match task.await {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(SharedString::from(error.to_string())),
                Err(error) => Err(SharedString::from(format!("catalog task failed: {error}"))),
            };
            let updated = this.update(cx, |this, cx| {
                // The connections changed meanwhile: this answer may describe
                // another server.
                if this.generation == generation {
                    done(this, result, cx);
                    this.tree_version += 1;
                    cx.notify();
                }
            });
            if let Err(error) = updated {
                tracing::debug!(%error, "explorer closed before the catalog query finished");
            }
        })
        .detach();
    }

    fn load_databases(&mut self, connection: &SharedString, cx: &mut Context<Self>) {
        self.tree_version += 1;
        self.databases.insert(connection.clone(), Load::Loading);
        let key = connection.clone();
        self.run_catalog(
            connection,
            cx,
            |tds| Box::pin(tds.list_databases()),
            move |this, result, cx| {
                let load = match result {
                    Ok(names) => {
                        let names: Arc<Vec<SharedString>> =
                            Arc::new(names.into_iter().map(SharedString::from).collect());
                        // The tabs' database pickers show the same list.
                        schema_cache::set_databases(cx, &key, names.clone());
                        Load::Loaded(names)
                    }
                    Err(message) => Load::Failed(message),
                };
                this.databases.insert(key, load);
            },
        );
    }

    fn load_objects(&mut self, key: (SharedString, SharedString), cx: &mut Context<Self>) {
        self.tree_version += 1;
        self.objects.insert(key.clone(), Load::Loading);
        let database = key.1.to_string();
        let connection = key.0.clone();
        self.run_catalog(
            &connection,
            cx,
            move |tds| Box::pin(async move { tds.list_objects(&database).await }),
            move |this, result, _| {
                let load = match result {
                    Ok(objects) => Load::Loaded(Arc::new(objects)),
                    Err(message) => Load::Failed(message),
                };
                this.objects.insert(key, load);
            },
        );
    }

    fn load_columns(&mut self, key: ObjectKey, cx: &mut Context<Self>) {
        self.tree_version += 1;
        self.columns.insert(key.clone(), Load::Loading);
        let database = key.database.to_string();
        let object = key.catalog_object();
        let connection = key.connection.clone();
        self.run_catalog(
            &connection,
            cx,
            move |tds| Box::pin(async move { tds.list_columns(&database, &object).await }),
            move |this, result, _| {
                let load = match result {
                    Ok(columns) => Load::Loaded(Arc::new(columns)),
                    Err(message) => Load::Failed(message),
                };
                this.columns.insert(key, load);
            },
        );
    }

    fn refresh(&mut self, id: &NodeId, cx: &mut Context<Self>) {
        self.tree_version += 1;
        match id {
            NodeId::Connection(connection) => {
                schema_cache::invalidate(cx, connection, None);
                self.databases.remove(connection);
                self.objects.retain(|(owner, _), _| owner != connection);
                self.columns.retain(|key, _| &key.connection != connection);
            }
            NodeId::Database(connection, database) => {
                schema_cache::invalidate(cx, connection, Some(database));
                self.objects.remove(&(connection.clone(), database.clone()));
                self.columns
                    .retain(|key, _| !(&key.connection == connection && &key.database == database));
            }
            NodeId::Group(connection, database, _) | NodeId::Schema(connection, database, _, _) => {
                return self.refresh(&NodeId::Database(connection.clone(), database.clone()), cx);
            }
            NodeId::Object(key) => {
                self.columns.remove(key);
            }
        }
        if self.expanded.contains(id) {
            self.ensure_loaded(id, cx);
        }
        cx.notify();
    }

    fn select_top(&mut self, key: &ObjectKey, cx: &mut Context<Self>) {
        let columns = self.loaded_columns(key);
        let sql = sql_db::select_script(
            Self::dialect(&key.connection, cx),
            &key.catalog_object(),
            columns.as_deref().map(Vec::as_slice),
            Some(SELECT_TOP_ROWS),
        );
        cx.emit(ExplorerEvent::OpenQuery {
            connection: key.connection.clone(),
            database: key.database.clone(),
            sql,
            run: true,
        });
    }

    fn loaded_columns(&self, key: &ObjectKey) -> Option<Arc<Vec<ColumnInfo>>> {
        match self.columns.get(key) {
            Some(Load::Loaded(columns)) => Some(columns.clone()),
            Some(Load::Loading | Load::Failed(_)) | None => None,
        }
    }

    fn script_select(&mut self, key: &ObjectKey, cx: &mut Context<Self>) {
        let dialect = Self::dialect(&key.connection, cx);
        let database = key.database.to_string();
        let object = key.catalog_object();
        let event_key = key.clone();
        self.run_catalog(
            &key.connection,
            cx,
            move |tds| Box::pin(async move { tds.list_columns(&database, &object).await }),
            move |_, result, cx| {
                let columns = result.ok();
                let sql = sql_db::select_script(
                    dialect,
                    &event_key.catalog_object(),
                    columns.as_deref(),
                    None,
                );
                cx.emit(ExplorerEvent::OpenQuery {
                    connection: event_key.connection.clone(),
                    database: event_key.database,
                    sql,
                    run: false,
                });
            },
        );
    }

    fn script_create(&mut self, key: &ObjectKey, cx: &mut Context<Self>) {
        let dialect = Self::dialect(&key.connection, cx);
        let database = key.database.to_string();
        let object = key.catalog_object();
        let event_key = key.clone();
        self.run_catalog(
            &key.connection,
            cx,
            move |connection| {
                Box::pin(async move {
                    // SQLite and Snowflake give a table's own DDL; for the others
                    // it is built from the columns.
                    if object.kind == ObjectKind::Table
                        && !matches!(dialect, Dialect::Sqlite | Dialect::Snowflake)
                    {
                        let columns = connection.list_columns(&database, &object).await?;
                        return Ok(Some(sql_db::create_table_script(
                            dialect, &object, &columns,
                        )));
                    }
                    connection.object_definition(&database, &object).await
                })
            },
            move |_, result, cx| {
                let sql = match result {
                    Ok(Some(script)) => script,
                    Ok(None) => format!(
                        "-- No definition available for {} (encrypted or not a module).\n",
                        event_key.catalog_object().qualified_name(dialect)
                    ),
                    Err(message) => format!("-- Could not script {}: {message}\n", event_key.name),
                };
                cx.emit(ExplorerEvent::OpenQuery {
                    connection: event_key.connection.clone(),
                    database: event_key.database,
                    sql,
                    run: false,
                });
            },
        );
    }

    fn on_row_click(&mut self, index: usize, event: &ClickEvent, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(index) else {
            return;
        };
        if let RowContent::Column(connection, column) = &row.content
            && event.click_count() == 2
        {
            let dialect = Self::dialect(connection, cx);
            cx.emit(ExplorerEvent::InsertText(
                dialect.quote_identifier(&column.name),
            ));
            return;
        }
        let RowContent::Node(id) = &row.content else {
            return;
        };
        let id = id.clone();
        self.selected = Some(id.clone());
        if event.click_count() == 2
            && let NodeId::Object(key) = &id
        {
            let dialect = Self::dialect(&key.connection, cx);
            cx.emit(ExplorerEvent::InsertText(
                key.catalog_object().qualified_name(dialect),
            ));
            return;
        }
        if let NodeId::Connection(connection) = &id {
            cx.emit(ExplorerEvent::UseConnection {
                connection: connection.clone(),
                database: None,
            });
        }
        if row.expanded.is_some() {
            self.toggle(&id, cx);
        }
        cx.notify();
    }

    fn build_rows(&self, cx: &Context<Self>) -> Vec<Row> {
        let filter = self.filter_input.read(cx).value().trim().to_lowercase();
        let group_by_schema = settings::get(cx).explorer_group_by_schema;
        let mut rows = Vec::new();
        for saved in connections::all(cx) {
            let connection = SharedString::from(saved.name.clone());
            let id = NodeId::Connection(connection.clone());
            let expanded = self.expanded.contains(&id);
            rows.push(node_row(id, 0, connection.clone(), Some(expanded)));
            if !expanded {
                continue;
            }
            let Some(databases) = Self::load_rows(self.databases.get(&connection), 1, &mut rows)
            else {
                continue;
            };
            for database in databases.iter() {
                let id = NodeId::Database(connection.clone(), database.clone());
                let expanded = self.expanded.contains(&id);
                rows.push(node_row(id, 1, database.clone(), Some(expanded)));
                if !expanded {
                    continue;
                }
                let key = (connection.clone(), database.clone());
                let Some(objects) = Self::load_rows(self.objects.get(&key), 2, &mut rows) else {
                    continue;
                };
                for kind in ObjectKind::ALL {
                    self.push_group(
                        &connection,
                        database,
                        kind,
                        &objects,
                        &filter,
                        group_by_schema,
                        &mut rows,
                    );
                }
            }
        }
        rows
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one tree level; a struct would only rename them"
    )]
    fn push_group(
        &self,
        connection: &SharedString,
        database: &SharedString,
        kind: ObjectKind,
        objects: &[CatalogObject],
        filter: &str,
        group_by_schema: bool,
        rows: &mut Vec<Row>,
    ) {
        let matching: Vec<&CatalogObject> = objects
            .iter()
            .filter(|object| object.kind == kind && matches_filter(object, filter))
            .collect();
        if matching.is_empty() && !filter.is_empty() {
            return;
        }
        let id = NodeId::Group(connection.clone(), database.clone(), kind);
        let expanded = self.expanded.contains(&id);
        let label = format!("{} ({})", kind.plural_label(), matching.len());
        rows.push(node_row(id, 2, label.into(), Some(expanded)));
        if !expanded {
            return;
        }
        if matching.is_empty() {
            rows.push(info_row(RowContent::Empty, 3, "None"));
            return;
        }
        if !group_by_schema {
            for object in matching {
                let label = format!("{}.{}", object.schema, object.name);
                self.push_object(connection, database, object, 3, label.into(), rows);
            }
            return;
        }
        let mut schemas: Vec<&str> = matching
            .iter()
            .map(|object| object.schema.as_str())
            .collect();
        schemas.dedup();
        for schema in schemas {
            let schema = SharedString::from(schema.to_owned());
            let id = NodeId::Schema(connection.clone(), database.clone(), kind, schema.clone());
            let expanded = self.expanded.contains(&id) || !filter.is_empty();
            rows.push(node_row(id, 3, schema.clone(), Some(expanded)));
            if !expanded {
                continue;
            }
            for object in matching
                .iter()
                .filter(|object| object.schema == schema.as_ref())
            {
                self.push_object(
                    connection,
                    database,
                    object,
                    4,
                    object.name.clone().into(),
                    rows,
                );
            }
        }
    }

    fn push_object(
        &self,
        connection: &SharedString,
        database: &SharedString,
        object: &CatalogObject,
        depth: usize,
        label: SharedString,
        rows: &mut Vec<Row>,
    ) {
        let key = ObjectKey {
            connection: connection.clone(),
            database: database.clone(),
            schema: object.schema.clone().into(),
            name: object.name.clone().into(),
            kind: object.kind,
        };
        let has_columns = matches!(object.kind, ObjectKind::Table | ObjectKind::View);
        let id = NodeId::Object(key.clone());
        let expanded = has_columns && self.expanded.contains(&id);
        rows.push(node_row(id, depth, label, has_columns.then_some(expanded)));
        if !expanded {
            return;
        }
        let Some(columns) = Self::load_rows(self.columns.get(&key), depth + 1, rows) else {
            return;
        };
        for column in columns.iter() {
            let null = if column.nullable { "null" } else { "not null" };
            rows.push(Row {
                label: format!("{} ({}, {null})", column.name, column.type_name).into(),
                content: RowContent::Column(key.connection.clone(), column.clone()),
                depth: depth + 1,
                expanded: None,
            });
        }
    }

    /// Pushes a loading or error row and returns `None` unless the data is loaded.
    fn load_rows<T: Clone>(load: Option<&Load<T>>, depth: usize, rows: &mut Vec<Row>) -> Option<T> {
        match load {
            Some(Load::Loaded(value)) => Some(value.clone()),
            Some(Load::Failed(message)) => {
                rows.push(info_row(RowContent::Error, depth, message.clone()));
                None
            }
            Some(Load::Loading) | None => {
                rows.push(info_row(RowContent::Loading, depth, "Loading…"));
                None
            }
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one menu per node kind; splitting would scatter them"
    )]
    fn context_menu_for(
        id: &NodeId,
        menu: PopupMenu,
        explorer: &WeakEntity<Self>,
        cx: &App,
    ) -> PopupMenu {
        let item = |label: &'static str, action: MenuAction| {
            let explorer = explorer.clone();
            PopupMenuItem::new(label).on_click(move |_, _, cx| {
                let updated = explorer.update(cx, |this, cx| action(this, cx));
                if let Err(error) = updated {
                    tracing::debug!(%error, "explorer closed before the menu action");
                }
            })
        };
        match id.clone() {
            NodeId::Connection(connection) => {
                let saved = connections::find(cx, &connection).cloned();
                let delete_name = connection;
                let refresh_id = id.clone();
                let mut menu = menu.item(item(
                    "Refresh",
                    Box::new(move |this, cx| this.refresh(&refresh_id, cx)),
                ));
                if let Some(saved) = saved {
                    menu = menu.item(item(
                        "Edit connection",
                        Box::new(move |_, cx| {
                            cx.emit(ExplorerEvent::EditConnection(saved.clone()));
                        }),
                    ));
                }
                menu.item(item(
                    "Delete connection",
                    Box::new(move |_, cx| {
                        cx.emit(ExplorerEvent::DeleteConnection(delete_name.clone()));
                    }),
                ))
            }
            NodeId::Database(connection, database) => {
                let new_query = (connection.clone(), database.clone());
                let use_here = (connection, database);
                let refresh_id = id.clone();
                menu.item(item(
                    "New Query",
                    Box::new(move |_, cx| {
                        cx.emit(ExplorerEvent::OpenQuery {
                            connection: new_query.0.clone(),
                            database: new_query.1.clone(),
                            sql: String::new(),
                            run: false,
                        });
                    }),
                ))
                .item(item(
                    "Use in current tab",
                    Box::new(move |_, cx| {
                        cx.emit(ExplorerEvent::UseConnection {
                            connection: use_here.0.clone(),
                            database: Some(use_here.1.clone()),
                        });
                    }),
                ))
                .separator()
                .item(item(
                    "Refresh",
                    Box::new(move |this, cx| this.refresh(&refresh_id, cx)),
                ))
            }
            NodeId::Group(..) | NodeId::Schema(..) => {
                let refresh_id = id.clone();
                menu.item(item(
                    "Refresh",
                    Box::new(move |this, cx| this.refresh(&refresh_id, cx)),
                ))
            }
            NodeId::Object(key) => {
                let name = key
                    .catalog_object()
                    .qualified_name(Self::dialect(&key.connection, cx));
                let mut menu = menu;
                if matches!(key.kind, ObjectKind::Table | ObjectKind::View) {
                    let top_key = key.clone();
                    let select_key = key.clone();
                    menu = menu
                        .item(item(
                            "Select Top 1000 Rows",
                            Box::new(move |this, cx| this.select_top(&top_key, cx)),
                        ))
                        .item(item(
                            "Script as SELECT",
                            Box::new(move |this, cx| this.script_select(&select_key, cx)),
                        ));
                }
                let create_key = key;
                let refresh_id = id.clone();
                menu.item(item(
                    "Script as CREATE",
                    Box::new(move |this, cx| this.script_create(&create_key, cx)),
                ))
                .separator()
                .item(item(
                    "Copy name",
                    Box::new(move |_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(name.clone()));
                    }),
                ))
                .item(item(
                    "Refresh",
                    Box::new(move |this, cx| this.refresh(&refresh_id, cx)),
                ))
            }
        }
    }

    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let explorer = cx.weak_entity();
        range
            .filter_map(|index| {
                let row = self.rows.get(index)?;
                let indent = px(ROW_INDENT * f32::from(u16::try_from(row.depth).unwrap_or(u16::MAX)));
                let chevron = match row.expanded {
                    Some(true) => "▾",
                    Some(false) => "▸",
                    None => " ",
                };
                let is_selected = matches!(&row.content, RowContent::Node(id) if self.selected.as_ref() == Some(id));
                let text_color = match &row.content {
                    RowContent::Error => cx.theme().danger,
                    RowContent::Loading | RowContent::Empty | RowContent::Column(..) => {
                        cx.theme().muted_foreground
                    }
                    RowContent::Node(_) => cx.theme().foreground,
                };
                let base = h_flex()
                    .id(("explorer-row", index))
                    .w_full()
                    .h(px(24.))
                    .pl(indent + px(4.))
                    .pr_1()
                    .gap_1()
                    .text_sm()
                    .text_color(text_color)
                    .when(is_selected, |row| row.bg(cx.theme().accent))
                    .hover(|row| row.bg(cx.theme().list_hover))
                    .child(div().w(px(10.)).flex_shrink_0().child(chevron))
                    .child(div().truncate().child(row.label.clone()))
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        this.on_row_click(index, event, cx);
                    }));
                let element = match &row.content {
                    RowContent::Node(id) => {
                        let id = id.clone();
                        let explorer = explorer.clone();
                        base.context_menu(move |menu, _, cx| {
                            Self::context_menu_for(&id, menu, &explorer, cx)
                        })
                        .into_any_element()
                    }
                    RowContent::Column(..) | RowContent::Loading | RowContent::Error | RowContent::Empty => {
                        base.into_any_element()
                    }
                };
                Some(element)
            })
            .collect()
    }
}

impl Render for ObjectExplorer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let group_by_schema = settings::get(cx).explorer_group_by_schema;
        // The workspace re-renders on every keystroke in a tab; the tree rarely changes.
        let built_for = (
            self.filter_input.read(cx).value().trim().to_lowercase(),
            group_by_schema,
            self.tree_version,
        );
        if self.rows_built_for.as_ref() != Some(&built_for) {
            self.rows = self.build_rows(cx);
            self.rows_built_for = Some(built_for);
        }
        let row_count = self.rows.len();
        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .child(
                h_flex()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("CONNECTIONS")
                    .child(
                        h_flex()
                            .child(
                                Button::new("group-by-schema")
                                    .ghost()
                                    .xsmall()
                                    .label(if group_by_schema { "By schema" } else { "Flat" })
                                    .tooltip("Group objects by schema, or list them as schema.name")
                                    .on_click(|_, _, cx| {
                                        settings::update(cx, |settings| {
                                            settings.explorer_group_by_schema =
                                                !settings.explorer_group_by_schema;
                                        });
                                        cx.refresh_windows();
                                    }),
                            )
                            .child(
                                Button::new("add-connection")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Plus)
                                    .tooltip("Add a connection")
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(ExplorerEvent::AddConnection);
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .pb_1()
                    .child(Input::new(&self.filter_input).xsmall()),
            )
            .when(connections::all(cx).is_empty(), |sidebar| {
                sidebar.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("Add a connection with +"),
                )
            })
            .child(
                uniform_list(
                    "explorer-rows",
                    row_count,
                    cx.processor(|this, range: Range<usize>, _, cx| this.render_rows(range, cx)),
                )
                .flex_1()
                .min_h_0(),
            )
    }
}

fn node_row(id: NodeId, depth: usize, label: SharedString, expanded: Option<bool>) -> Row {
    Row {
        content: RowContent::Node(id),
        depth,
        label,
        expanded,
    }
}

fn info_row(content: RowContent, depth: usize, label: impl Into<SharedString>) -> Row {
    Row {
        content,
        depth,
        label: label.into(),
        expanded: None,
    }
}

fn matches_filter(object: &CatalogObject, filter: &str) -> bool {
    filter.is_empty()
        || object.name.to_lowercase().contains(filter)
        || format!("{}.{}", object.schema, object.name)
            .to_lowercase()
            .contains(filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_by_name_or_schema_and_name() {
        let object = CatalogObject {
            schema: "Sales".to_owned(),
            name: "OrderLines".to_owned(),
            kind: ObjectKind::Table,
        };
        assert!(matches_filter(&object, ""));
        assert!(matches_filter(&object, "orderl"));
        assert!(matches_filter(&object, "sales.order"));
        assert!(!matches_filter(&object, "dbo.order"));
    }
}
