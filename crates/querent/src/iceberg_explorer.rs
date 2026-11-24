//! Sidebar tree of Iceberg REST catalogs: namespaces, tables, and each table's
//! metadata down to its manifests and data files. Each level loads when opened.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Entity, EventEmitter,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, UniformListScrollHandle, WeakEntity,
    Window, div, prelude::FluentBuilder as _, px, uniform_list,
};
use sql_db::ResultSet;
use sql_iceberg::{
    Catalog, CatalogConfig, FileInfo, ManifestInfo, PreviewScope, SnapshotInfo, TableInfo,
};

use crate::session::SavedCatalog;
use crate::{secrets, tokio_bridge};

const ROW_INDENT: f32 = 12.;

pub enum IcebergEvent {
    /// Shows `result` in a new tab, under a comment naming what it is.
    ShowResult { title: String, result: ResultSet },
    /// A catalog was added, edited or removed.
    CatalogsChanged,
}

impl EventEmitter<IcebergEvent> for IcebergExplorer {}

enum Listing<T> {
    Loading,
    Loaded(T),
    Failed(SharedString),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TableKey {
    catalog: String,
    namespace: Vec<String>,
    name: String,
}

impl TableKey {
    fn qualified(&self) -> String {
        format!(
            "{}.{}.{}",
            self.catalog,
            self.namespace.join("."),
            self.name
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Section {
    Schema,
    Partition,
    Snapshots,
    Properties,
    MetadataFiles,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TreeId {
    Catalog(String),
    Namespace(String, Vec<String>),
    Table(TableKey),
    Section(TableKey, Section),
    Snapshot(TableKey, i64),
    Manifest(TableKey, i64, String),
    /// A file of a snapshot; `holds_rows` is false for delete files.
    File {
        table: TableKey,
        snapshot: i64,
        path: String,
        holds_rows: bool,
    },
}

impl TreeId {
    fn catalog(&self) -> &str {
        match self {
            TreeId::Catalog(catalog) | TreeId::Namespace(catalog, _) => catalog,
            TreeId::Table(key)
            | TreeId::Section(key, _)
            | TreeId::Snapshot(key, _)
            | TreeId::Manifest(key, _, _)
            | TreeId::File { table: key, .. } => &key.catalog,
        }
    }
}

/// Something an open row needs that has not been asked for yet.
enum Need {
    Connect(String),
    Namespaces(String, Vec<String>),
    Tables(String, Vec<String>),
    Table(TableKey),
    Manifests(TableKey, i64),
    Files(TableKey, i64, String),
}

enum RowContent {
    Tree(TreeId),
    /// A leaf such as a column or property; right-click copies `copy`.
    Value {
        copy: String,
    },
    Info,
    Error,
}

struct Row {
    content: RowContent,
    depth: usize,
    label: SharedString,
    detail: Option<SharedString>,
    expanded: Option<bool>,
}

struct CatalogEntry {
    saved: SavedCatalog,
    connection: Option<Listing<Arc<Catalog>>>,
    /// Replaced on reload, edit and removal, so an older answer is dropped.
    generation: u64,
}

type NamespaceKey = (String, Vec<String>);
type ManifestKey = (TableKey, i64);
type FileKey = (TableKey, i64, String);

type MenuAction = Box<dyn Fn(&mut IcebergExplorer, &mut Window, &mut Context<IcebergExplorer>)>;

pub struct IcebergExplorer {
    catalogs: Vec<CatalogEntry>,
    namespaces: HashMap<NamespaceKey, Listing<Vec<Vec<String>>>>,
    tables: HashMap<NamespaceKey, Listing<Vec<String>>>,
    loaded: HashMap<TableKey, Listing<Arc<TableInfo>>>,
    manifests: HashMap<ManifestKey, Listing<Arc<Vec<ManifestInfo>>>>,
    files: HashMap<FileKey, Listing<Arc<Vec<FileInfo>>>>,
    expanded: HashSet<TreeId>,
    selected: Option<TreeId>,
    rows: Vec<Row>,
    needs: Vec<Need>,
    tree_version: u64,
    rows_built_for: Option<u64>,
    scroll: UniformListScrollHandle,
    /// Source of generations; never reused, so a re-added catalog ignores the removed one's answers.
    last_generation: u64,
    /// Per table, the generation its last Reload set.
    table_generations: HashMap<TableKey, u64>,
    /// Keychain writes run one after another, so a remove cannot land after a later add.
    secret_queue: Option<Task<()>>,
}

fn secret_key(catalog: &str) -> String {
    format!("iceberg-catalog:{catalog}")
}

/// A `file:` location as a local path, for Reveal in File Manager.
fn local_path(location: &str) -> Option<PathBuf> {
    let path = location
        .strip_prefix("file://")
        .or_else(|| location.strip_prefix("file:"))
        .unwrap_or(location);
    path.starts_with('/').then(|| PathBuf::from(path))
}

impl IcebergExplorer {
    pub fn new() -> Self {
        Self {
            catalogs: Vec::new(),
            namespaces: HashMap::new(),
            tables: HashMap::new(),
            loaded: HashMap::new(),
            manifests: HashMap::new(),
            files: HashMap::new(),
            expanded: HashSet::new(),
            selected: None,
            rows: Vec::new(),
            needs: Vec::new(),
            tree_version: 0,
            rows_built_for: None,
            scroll: UniformListScrollHandle::new(),
            last_generation: 0,
            table_generations: HashMap::new(),
            secret_queue: None,
        }
    }

    fn next_generation(&mut self) -> u64 {
        self.last_generation += 1;
        self.last_generation
    }

    /// Runs `work` after every keychain change queued before it.
    fn queue_secret_work(
        &mut self,
        work: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) -> tokio::sync::oneshot::Receiver<anyhow::Result<()>> {
        let previous = self.secret_queue.take();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.secret_queue = Some(cx.background_spawn(async move {
            if let Some(previous) = previous {
                previous.await;
            }
            if sender.send(work()).is_err() {
                tracing::debug!("nobody waited for a keychain change");
            }
        }));
        receiver
    }

    pub fn has_catalogs(&self) -> bool {
        !self.catalogs.is_empty()
    }

    pub fn saved_catalogs(&self) -> Vec<SavedCatalog> {
        self.catalogs
            .iter()
            .map(|entry| entry.saved.clone())
            .collect()
    }

    pub fn restore(&mut self, saved: Vec<SavedCatalog>, cx: &mut Context<Self>) {
        self.catalogs = Vec::new();
        for saved in saved {
            let generation = self.next_generation();
            self.catalogs.push(CatalogEntry {
                saved,
                connection: None,
                generation,
            });
        }
        self.changed(cx);
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.tree_version += 1;
        cx.notify();
    }

    fn entry(&self, catalog: &str) -> Option<&CatalogEntry> {
        self.catalogs
            .iter()
            .find(|entry| entry.saved.name == catalog)
    }

    fn generation(&self, catalog: &str) -> Option<u64> {
        self.entry(catalog).map(|entry| entry.generation)
    }

    fn connected(&self, catalog: &str) -> Option<Arc<Catalog>> {
        match self.entry(catalog)?.connection.as_ref()? {
            Listing::Loaded(connection) => Some(connection.clone()),
            Listing::Loading | Listing::Failed(_) => None,
        }
    }

    fn table(&self, key: &TableKey) -> Option<Arc<TableInfo>> {
        match self.loaded.get(key)? {
            Listing::Loaded(table) => Some(table.clone()),
            Listing::Loading | Listing::Failed(_) => None,
        }
    }

    /// Drops everything loaded from `catalog`, so it is read again when shown.
    fn forget(&mut self, catalog: &str) {
        self.namespaces.retain(|(name, _), _| name != catalog);
        self.tables.retain(|(name, _), _| name != catalog);
        self.loaded.retain(|key, _| key.catalog != catalog);
        self.manifests.retain(|(key, _), _| key.catalog != catalog);
        self.files.retain(|(key, _, _), _| key.catalog != catalog);
        let generation = self.next_generation();
        if let Some(entry) = self
            .catalogs
            .iter_mut()
            .find(|entry| entry.saved.name == catalog)
        {
            entry.connection = None;
            entry.generation = generation;
        }
    }

    fn reload(&mut self, catalog: &str, cx: &mut Context<Self>) {
        self.forget(catalog);
        self.changed(cx);
    }

    fn reload_table(&mut self, key: &TableKey, cx: &mut Context<Self>) {
        let generation = self.next_generation();
        self.table_generations.insert(key.clone(), generation);
        self.loaded.remove(key);
        self.manifests.retain(|(table, _), _| table != key);
        self.files.retain(|(table, _, _), _| table != key);
        self.changed(cx);
    }

    fn remove(&mut self, catalog: &str, cx: &mut Context<Self>) {
        self.forget(catalog);
        self.catalogs.retain(|entry| entry.saved.name != catalog);
        self.expanded.retain(|id| id.catalog() != catalog);
        let key = secret_key(catalog);
        // Nobody waits on the outcome; a failure is logged and the stale entry is harmless.
        drop(self.queue_secret_work(
            move || {
                if let Err(error) = secrets::set(&key, None) {
                    tracing::warn!(%error, "could not delete an Iceberg catalog's credential");
                }
                Ok(())
            },
            cx,
        ));
        cx.emit(IcebergEvent::CatalogsChanged);
        self.changed(cx);
    }

    fn toggle(&mut self, id: &TreeId, cx: &mut Context<Self>) {
        if self.expanded.remove(id) {
            self.forget_failure(id);
        } else {
            self.expanded.insert(id.clone());
        }
        self.changed(cx);
    }

    /// Closing a row that failed to load lets the next open try again.
    fn forget_failure(&mut self, id: &TreeId) {
        fn failed<T>(listing: Option<&Listing<T>>) -> bool {
            matches!(listing, Some(Listing::Failed(_)))
        }
        match id {
            TreeId::Catalog(catalog) => {
                let connection_failed = self
                    .entry(catalog)
                    .is_some_and(|entry| failed(entry.connection.as_ref()));
                if connection_failed {
                    self.forget(catalog);
                }
            }
            TreeId::Namespace(catalog, namespace) => {
                let key = (catalog.clone(), namespace.clone());
                if failed(self.namespaces.get(&key)) {
                    self.namespaces.remove(&key);
                }
                if failed(self.tables.get(&key)) {
                    self.tables.remove(&key);
                }
            }
            TreeId::Table(key) => {
                if failed(self.loaded.get(key)) {
                    self.loaded.remove(key);
                }
            }
            TreeId::Snapshot(key, snapshot) => {
                let key = (key.clone(), *snapshot);
                if failed(self.manifests.get(&key)) {
                    self.manifests.remove(&key);
                }
            }
            TreeId::Manifest(key, snapshot, path) => {
                let key = (key.clone(), *snapshot, path.clone());
                if failed(self.files.get(&key)) {
                    self.files.remove(&key);
                }
            }
            TreeId::Section(..) | TreeId::File { .. } => {}
        }
    }

    fn on_row_click(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(index) else {
            return;
        };
        let RowContent::Tree(id) = &row.content else {
            return;
        };
        let id = id.clone();
        let expandable = row.expanded.is_some();
        self.selected = Some(id.clone());
        if expandable {
            self.toggle(&id, cx);
        } else {
            self.changed(cx);
        }
    }

    /// Starts the loads the last build of the rows found missing.
    fn load_needs(&mut self, cx: &mut Context<Self>) {
        for need in std::mem::take(&mut self.needs) {
            match need {
                Need::Connect(catalog) => self.connect(&catalog, cx),
                Need::Namespaces(catalog, parent) => {
                    let Some(connection) = self.connected(&catalog) else {
                        continue;
                    };
                    let key = (catalog.clone(), parent.clone());
                    self.namespaces.insert(key.clone(), Listing::Loading);
                    self.spawn_load(
                        &catalog,
                        None,
                        async move { connection.namespaces(&parent).await },
                        move |this, listing| {
                            this.namespaces.insert(key, listing);
                        },
                        cx,
                    );
                }
                Need::Tables(catalog, namespace) => {
                    let Some(connection) = self.connected(&catalog) else {
                        continue;
                    };
                    let key = (catalog.clone(), namespace.clone());
                    self.tables.insert(key.clone(), Listing::Loading);
                    self.spawn_load(
                        &catalog,
                        None,
                        async move { connection.tables(&namespace).await },
                        move |this, listing| {
                            this.tables.insert(key, listing);
                        },
                        cx,
                    );
                }
                Need::Table(key) => {
                    let Some(connection) = self.connected(&key.catalog) else {
                        continue;
                    };
                    self.loaded.insert(key.clone(), Listing::Loading);
                    let (namespace, name) = (key.namespace.clone(), key.name.clone());
                    let (catalog, table) = (key.catalog.clone(), key.clone());
                    self.spawn_load(
                        &catalog,
                        Some(&table),
                        async move { connection.table(&namespace, &name).await.map(Arc::new) },
                        move |this, listing| {
                            this.loaded.insert(key, listing);
                        },
                        cx,
                    );
                }
                Need::Manifests(key, snapshot) => {
                    let Some(table) = self.table(&key) else {
                        continue;
                    };
                    let catalog = key.catalog.clone();
                    let table_key = key.clone();
                    let slot = (key, snapshot);
                    self.manifests.insert(slot.clone(), Listing::Loading);
                    self.spawn_load(
                        &catalog,
                        Some(&table_key),
                        async move { table.manifests(snapshot).await.map(Arc::new) },
                        move |this, listing| {
                            this.manifests.insert(slot, listing);
                        },
                        cx,
                    );
                }
                Need::Files(key, snapshot, path) => {
                    let Some(table) = self.table(&key) else {
                        continue;
                    };
                    let manifest = match self.manifests.get(&(key.clone(), snapshot)) {
                        Some(Listing::Loaded(manifests)) => manifests
                            .iter()
                            .find(|manifest| manifest.path == path)
                            .cloned(),
                        Some(Listing::Loading | Listing::Failed(_)) | None => None,
                    };
                    let Some(manifest) = manifest else {
                        continue;
                    };
                    let catalog = key.catalog.clone();
                    let table_key = key.clone();
                    let slot = (key, snapshot, path);
                    self.files.insert(slot.clone(), Listing::Loading);
                    self.spawn_load(
                        &catalog,
                        Some(&table_key),
                        async move { table.files(&manifest).await.map(Arc::new) },
                        move |this, listing| {
                            this.files.insert(slot, listing);
                        },
                        cx,
                    );
                }
            }
        }
    }

    /// Runs `work` on the tokio runtime and hands its outcome to `store`, unless
    /// the catalog, or the table it reads, was reloaded or removed in the meantime.
    fn spawn_load<T: Send + 'static>(
        &self,
        catalog: &str,
        table: Option<&TableKey>,
        work: impl Future<Output = sql_iceberg::Result<T>> + Send + 'static,
        store: impl FnOnce(&mut Self, Listing<T>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(generation) = self.generation(catalog) else {
            return;
        };
        let catalog = catalog.to_owned();
        let table = table.map(|key| (key.clone(), self.table_generation(key)));
        let task = tokio_bridge::spawn(cx, work);
        cx.spawn(async move |this, cx| {
            let listing = match task.await {
                Ok(Ok(value)) => Listing::Loaded(value),
                Ok(Err(error)) => Listing::Failed(error.to_string().into()),
                Err(error) => Listing::Failed(format!("load failed: {error}").into()),
            };
            let updated = this.update(cx, |this, cx| {
                let table_current = table
                    .as_ref()
                    .is_none_or(|(key, generation)| this.table_generation(key) == *generation);
                if this.generation(&catalog) == Some(generation) && table_current {
                    store(this, listing);
                    this.changed(cx);
                }
            });
            if let Err(error) = updated {
                tracing::debug!(%error, "Iceberg tree closed during a load");
            }
        })
        .detach();
    }

    fn table_generation(&self, key: &TableKey) -> u64 {
        self.table_generations.get(key).copied().unwrap_or_default()
    }

    fn connect(&mut self, catalog: &str, cx: &mut Context<Self>) {
        let Some(entry) = self
            .catalogs
            .iter_mut()
            .find(|entry| entry.saved.name == catalog)
        else {
            return;
        };
        entry.connection = Some(Listing::Loading);
        let saved = entry.saved.clone();
        let key = secret_key(catalog);
        let generation = entry.generation;
        let task = tokio_bridge::spawn(cx, async move {
            let credential = match tokio::task::spawn_blocking(move || secrets::get(&key)).await {
                Ok(credential) => credential,
                Err(error) => return Err(format!("reading the credential failed: {error}")),
            };
            let config = CatalogConfig {
                uri: saved.uri,
                warehouse: (!saved.warehouse.is_empty()).then_some(saved.warehouse),
                credential,
            };
            Catalog::connect(&config)
                .await
                .map_err(|error| error.to_string())
        });
        let catalog = catalog.to_owned();
        cx.spawn(async move |this, cx| {
            let connection = match task.await {
                Ok(Ok(connection)) => Listing::Loaded(Arc::new(connection)),
                Ok(Err(error)) => Listing::Failed(error.into()),
                Err(error) => Listing::Failed(format!("connect failed: {error}").into()),
            };
            let updated =
                this.update(cx, |this, cx| {
                    let Some(entry) = this.catalogs.iter_mut().find(|entry| {
                        entry.saved.name == catalog && entry.generation == generation
                    }) else {
                        return;
                    };
                    entry.connection = Some(connection);
                    this.changed(cx);
                });
            if let Err(error) = updated {
                tracing::debug!(%error, "Iceberg tree closed while connecting");
            }
        })
        .detach();
    }

    fn show(title: String, result: ResultSet, cx: &mut Context<Self>) {
        cx.emit(IcebergEvent::ShowResult { title, result });
    }

    fn show_section(&mut self, key: &TableKey, section: Section, cx: &mut Context<Self>) {
        let Some(table) = self.table(key) else {
            return;
        };
        let (what, result) = match section {
            Section::Schema => ("schema", sql_iceberg::fields_result(&table.fields)),
            Section::Snapshots => (
                "snapshots",
                sql_iceberg::snapshots_result(&table.snapshots, table.current_snapshot),
            ),
            Section::Partition | Section::Properties | Section::MetadataFiles => return,
        };
        Self::show(format!("{} {what}", key.qualified()), result, cx);
    }

    /// Reads the manifests (`files == false`) or every file of a snapshot into a new tab.
    fn show_snapshot(
        &mut self,
        key: &TableKey,
        snapshot: i64,
        files: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(table) = self.table(key) else {
            return;
        };
        let title = format!(
            "{} snapshot {snapshot} {}",
            key.qualified(),
            if files { "files" } else { "manifests" }
        );
        let task = tokio_bridge::spawn(cx, async move {
            if files {
                table
                    .snapshot_files(snapshot)
                    .await
                    .map(|files| sql_iceberg::files_result(&files))
            } else {
                table
                    .manifests(snapshot)
                    .await
                    .map(|manifests| sql_iceberg::manifests_result(&manifests))
            }
        });
        Self::show_when_read(title, task, window, cx);
    }

    /// Reads up to the row limit from the table, a snapshot of it, or one data file.
    fn preview(
        &mut self,
        key: &TableKey,
        scope: PreviewScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(table) = self.table(key) else {
            return;
        };
        let title = match &scope {
            PreviewScope::Current => format!("{} rows", key.qualified()),
            PreviewScope::Snapshot(snapshot) => {
                format!("{} rows at snapshot {snapshot}", key.qualified())
            }
            PreviewScope::File { path, .. } => {
                format!(
                    "{} rows in {}",
                    key.qualified(),
                    sql_iceberg::file_name(path)
                )
            }
        };
        // 0 in Settings means no limit, as for queries.
        let limit = match crate::settings::get(cx).row_limit {
            0 => usize::MAX,
            limit => limit,
        };
        let task = tokio_bridge::spawn(cx, async move { table.preview(&scope, limit).await });
        Self::show_when_read(title, task, window, cx);
    }

    fn show_manifest_files(
        &mut self,
        key: &TableKey,
        snapshot: i64,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(table) = self.table(key) else {
            return;
        };
        let title = format!(
            "{} manifest {}",
            key.qualified(),
            sql_iceberg::file_name(path)
        );
        let path = path.to_owned();
        let task = tokio_bridge::spawn(cx, async move {
            let manifest = table.manifest(snapshot, &path).await?;
            table
                .files(&manifest)
                .await
                .map(|files| sql_iceberg::files_result(&files))
        });
        Self::show_when_read(title, task, window, cx);
    }

    fn show_when_read(
        title: String,
        task: gpui_kit::Task<anyhow::Result<sql_iceberg::Result<ResultSet>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let result = match task.await {
                Ok(read) => read.map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            let shown = this.update_in(cx, |_, window, cx| match result {
                Ok(result) => Self::show(title, result, cx),
                Err(error) => {
                    crate::workspace::notify_error(format!("{title}: {error}"), window, cx);
                }
            });
            if let Err(error) = shown {
                tracing::debug!(%error, "Iceberg tree closed while files were read");
            }
        })
        .detach();
    }

    /// Opens the dialog to add a catalog, or to edit `editing`.
    pub fn ask_catalog(editing: Option<SavedCatalog>, window: &mut Window, cx: &mut Context<Self>) {
        let text = |value: &str, placeholder: &'static str, window: &mut Window, cx: &mut App| {
            let value = value.to_owned();
            cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(value)
                    .placeholder(placeholder)
            })
        };
        let current = editing.clone().unwrap_or_default();
        let name = text(&current.name, "lake", window, cx);
        let uri = text(&current.uri, "http://localhost:8181", window, cx);
        let warehouse = text(&current.warehouse, "Optional", window, cx);
        let credential_hint = if editing.is_some() {
            "Unchanged"
        } else {
            "Optional: a token, or client_id:secret"
        };
        let credential = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(credential_hint)
                .masked(true)
        });
        let explorer = cx.weak_entity();
        let focus = name.clone();
        let title = if editing.is_some() {
            "Edit Iceberg Catalog"
        } else {
            "Add Iceberg Catalog"
        };
        window.open_dialog(cx, move |dialog, _, cx| {
            let muted = cx.theme().muted_foreground;
            let labeled = |label: &'static str, input: &Entity<InputState>| {
                v_flex()
                    .gap_1()
                    .child(div().text_sm().child(label))
                    .child(Input::new(input))
            };
            let (name, uri, warehouse, credential) =
                (name.clone(), uri.clone(), warehouse.clone(), credential.clone());
            let explorer = explorer.clone();
            let editing = editing.clone();
            dialog
                .title(title)
                .child(
                    v_flex()
                        .gap_3()
                        .child(labeled("Name", &name))
                        .child(labeled("REST catalog URI", &uri))
                        .child(labeled("Warehouse", &warehouse))
                        .child(labeled("Credential", &credential))
                        .child(div().text_xs().text_color(muted).child(
                            "The credential is kept in the system keychain. Data files are read from local disk.",
                        )),
                )
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().child(Button::new("cancel").label("Cancel").outline()))
                        .child(DialogAction::new().child(Button::new("ok").label("Save").primary())),
                )
                .on_ok(move |_, window, cx| {
                    let saved = SavedCatalog {
                        name: name.read(cx).value().trim().to_owned(),
                        uri: uri.read(cx).value().trim().to_owned(),
                        warehouse: warehouse.read(cx).value().trim().to_owned(),
                    };
                    let credential = credential.read(cx).value().trim().to_owned();
                    let editing = editing.clone();
                    let saved_it = explorer.update(cx, |this, cx| {
                        this.save_catalog(editing.as_ref(), saved, credential, window, cx)
                    });
                    saved_it.unwrap_or(true)
                })
        });
        window.defer(cx, move |window, cx| {
            focus.update(cx, |input, cx| input.focus(window, cx));
        });
    }

    /// Adds or replaces a catalog; false keeps the dialog open over a mistake.
    fn save_catalog(
        &mut self,
        editing: Option<&SavedCatalog>,
        saved: SavedCatalog,
        credential: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let old_name = editing.map(|old| old.name.clone());
        let problem = if saved.name.is_empty() {
            Some("Give the catalog a name.")
        } else if !(saved.uri.starts_with("http://") || saved.uri.starts_with("https://")) {
            Some("The URI starts with http:// or https://.")
        } else if old_name.as_deref() != Some(saved.name.as_str())
            && self.entry(&saved.name).is_some()
        {
            Some("Another catalog has that name.")
        } else {
            None
        };
        if let Some(problem) = problem {
            window.push_notification(problem, cx);
            return false;
        }
        let new_key = secret_key(&saved.name);
        let old_key = old_name.as_deref().map(secret_key);
        let typed = (!credential.is_empty()).then_some(credential);
        let renamed_from = old_key.filter(|old_key| *old_key != new_key);
        let stored = self.queue_secret_work(
            move || {
                // The new key is written before the old one goes, so a failure loses nothing.
                let moving = match (&typed, &renamed_from) {
                    (Some(_), _) => typed,
                    // An unreadable old credential stays where it is rather than being dropped.
                    (None, Some(old_key)) => secrets::get(old_key),
                    (None, None) => return Ok(()),
                };
                let Some(credential) = moving else {
                    return Ok(());
                };
                secrets::set(&new_key, Some(&credential))?;
                if let Some(old_key) = renamed_from {
                    secrets::set(&old_key, None)?;
                }
                Ok(())
            },
            cx,
        );
        let name = saved.name.clone();
        cx.spawn_in(window, async move |this, cx| {
            let stored = match stored.await {
                Ok(stored) => stored,
                Err(error) => Err(anyhow::anyhow!("the keychain task stopped: {error}")),
            };
            let updated = this.update_in(cx, |this, window, cx| {
                if let Err(error) = stored {
                    crate::workspace::notify_error(
                        format!("Could not store the credential for {name}: {error:#}"),
                        window,
                        cx,
                    );
                }
                // Connect with the credential that was just stored.
                this.reload(&name, cx);
            });
            if let Err(error) = updated {
                tracing::debug!(%error, "Iceberg tree closed while a credential was stored");
            }
        })
        .detach();
        if let Some(old_name) = &old_name {
            self.forget(old_name);
            self.expanded.retain(|id| id.catalog() != old_name);
        }
        if let Some(entry) = old_name.as_deref().and_then(|old_name| {
            self.catalogs
                .iter_mut()
                .find(|entry| entry.saved.name == old_name)
        }) {
            entry.saved = saved.clone();
        } else {
            let generation = self.next_generation();
            self.catalogs.push(CatalogEntry {
                saved: saved.clone(),
                connection: None,
                generation,
            });
        }
        self.expanded.insert(TreeId::Catalog(saved.name));
        cx.emit(IcebergEvent::CatalogsChanged);
        self.changed(cx);
        true
    }

    fn context_menu_for(
        &self,
        id: &TreeId,
        menu: PopupMenu,
        explorer: &WeakEntity<Self>,
    ) -> PopupMenu {
        let item = |label: &'static str, action: MenuAction| menu_item(explorer, label, action);
        match id {
            TreeId::Catalog(catalog) => {
                let Some(entry) = self.entry(catalog) else {
                    return menu;
                };
                let (reload, remove) = (catalog.clone(), catalog.clone());
                let saved = entry.saved.clone();
                menu.item(item(
                    "Reload",
                    Box::new(move |this, _, cx| this.reload(&reload, cx)),
                ))
                .item(item(
                    "Edit…",
                    Box::new(move |_, window, cx| {
                        Self::ask_catalog(Some(saved.clone()), window, cx);
                    }),
                ))
                .separator()
                .item(copy_item("Copy URI", entry.saved.uri.clone()))
                .separator()
                .item(item(
                    "Remove",
                    Box::new(move |this, _, cx| this.remove(&remove, cx)),
                ))
            }
            TreeId::Namespace(_, namespace) => {
                menu.item(copy_item("Copy Name", namespace.join(".")))
            }
            TreeId::Table(key) => self.table_menu(key, menu, explorer),
            TreeId::Section(key, section @ (Section::Schema | Section::Snapshots)) => {
                let (key, section) = (key.clone(), *section);
                menu.item(item(
                    "Open in Grid",
                    Box::new(move |this, _, cx| this.show_section(&key, section, cx)),
                ))
            }
            TreeId::Section(..) => menu,
            TreeId::Snapshot(key, snapshot) => {
                let (manifests, files, snapshot) = (key.clone(), key.clone(), *snapshot);
                let preview = key.clone();
                menu.item(item(
                    "Preview Rows at This Snapshot",
                    Box::new(move |this, window, cx| {
                        this.preview(&preview, PreviewScope::Snapshot(snapshot), window, cx);
                    }),
                ))
                .separator()
                .item(item(
                    "Open Manifests in Grid",
                    Box::new(move |this, window, cx| {
                        this.show_snapshot(&manifests, snapshot, false, window, cx);
                    }),
                ))
                .item(item(
                    "Open Files in Grid",
                    Box::new(move |this, window, cx| {
                        this.show_snapshot(&files, snapshot, true, window, cx);
                    }),
                ))
                .separator()
                .item(copy_item("Copy Snapshot ID", snapshot.to_string()))
            }
            TreeId::Manifest(key, snapshot, path) => {
                let (key, snapshot, shown) = (key.clone(), *snapshot, path.clone());
                menu.item(item(
                    "Open Files in Grid",
                    Box::new(move |this, window, cx| {
                        this.show_manifest_files(&key, snapshot, &shown, window, cx);
                    }),
                ))
                .separator()
                .item(copy_item("Copy Path", path.clone()))
            }
            TreeId::File {
                table,
                snapshot,
                path,
                holds_rows,
            } => Self::file_menu(table, *snapshot, path, *holds_rows, menu, explorer),
        }
    }

    fn file_menu(
        table: &TableKey,
        snapshot: i64,
        path: &str,
        holds_rows: bool,
        menu: PopupMenu,
        explorer: &WeakEntity<Self>,
    ) -> PopupMenu {
        let item = |label: &'static str, action: MenuAction| menu_item(explorer, label, action);
        let mut menu = menu;
        if holds_rows {
            let scope = PreviewScope::File {
                snapshot,
                path: path.to_owned(),
            };
            let table = table.clone();
            menu = menu
                .item(item(
                    "Preview Rows",
                    Box::new(move |this, window, cx| {
                        this.preview(&table, scope.clone(), window, cx);
                    }),
                ))
                .separator();
        }
        let menu = menu.item(copy_item("Copy Path", path.to_owned()));
        match local_path(path) {
            Some(local) => menu.item(
                PopupMenuItem::new("Reveal in File Manager")
                    .on_click(move |_, _, cx| cx.reveal_path(&local)),
            ),
            None => menu,
        }
    }

    fn table_menu(
        &self,
        key: &TableKey,
        menu: PopupMenu,
        explorer: &WeakEntity<Self>,
    ) -> PopupMenu {
        let item = |label: &'static str, action: MenuAction| menu_item(explorer, label, action);
        let mut menu = menu;
        if let Some(table) = self.table(key) {
            let (schema, snapshots, files) = (key.clone(), key.clone(), key.clone());
            if table.current_snapshot.is_some() {
                let preview = key.clone();
                menu = menu
                    .item(item(
                        "Preview Rows",
                        Box::new(move |this, window, cx| {
                            this.preview(&preview, PreviewScope::Current, window, cx);
                        }),
                    ))
                    .separator();
            }
            menu = menu
                .item(item(
                    "Open Schema in Grid",
                    Box::new(move |this, _, cx| {
                        this.show_section(&schema, Section::Schema, cx);
                    }),
                ))
                .item(item(
                    "Open Snapshots in Grid",
                    Box::new(move |this, _, cx| {
                        this.show_section(&snapshots, Section::Snapshots, cx);
                    }),
                ));
            if let Some(current) = table.current_snapshot {
                menu = menu.item(item(
                    "Open Current Files in Grid",
                    Box::new(move |this, window, cx| {
                        this.show_snapshot(&files, current, true, window, cx);
                    }),
                ));
            }
            menu = menu
                .separator()
                .item(copy_item("Copy Location", table.location.clone()));
            if let Some(metadata) = &table.metadata_location {
                menu = menu.item(copy_item("Copy Metadata File Path", metadata.clone()));
            }
        }
        let reload = key.clone();
        menu.item(copy_item("Copy Name", key.qualified()))
            .separator()
            .item(item(
                "Reload",
                Box::new(move |this, _, cx| this.reload_table(&reload, cx)),
            ))
    }

    fn build_rows(&mut self) -> Vec<Row> {
        let mut rows = Vec::new();
        let mut needs = Vec::new();
        for entry in &self.catalogs {
            let name = entry.saved.name.clone();
            let id = TreeId::Catalog(name.clone());
            let expanded = self.expanded.contains(&id);
            rows.push(Row {
                content: RowContent::Tree(id),
                depth: 0,
                label: name.clone().into(),
                detail: Some(entry.saved.uri.clone().into()),
                expanded: Some(expanded),
            });
            if !expanded {
                continue;
            }
            match &entry.connection {
                None => {
                    needs.push(Need::Connect(name));
                    rows.push(info_row(RowContent::Info, 1, "Connecting…"));
                }
                Some(Listing::Loading) => rows.push(info_row(RowContent::Info, 1, "Connecting…")),
                Some(Listing::Failed(message)) => {
                    rows.push(info_row(RowContent::Error, 1, message.clone()));
                }
                Some(Listing::Loaded(_)) => {
                    self.push_namespace(&name, &[], 1, &mut rows, &mut needs);
                }
            }
        }
        self.needs = needs;
        rows
    }

    /// The namespaces and tables under `parent` (the catalog's top level when empty).
    fn push_namespace(
        &self,
        catalog: &str,
        parent: &[String],
        depth: usize,
        rows: &mut Vec<Row>,
        needs: &mut Vec<Need>,
    ) {
        let key = (catalog.to_owned(), parent.to_vec());
        match self.namespaces.get(&key) {
            None => {
                needs.push(Need::Namespaces(key.0.clone(), key.1.clone()));
                rows.push(info_row(RowContent::Info, depth, "Loading…"));
            }
            Some(Listing::Loading) => rows.push(info_row(RowContent::Info, depth, "Loading…")),
            // Catalogs without nested namespaces refuse the listing under one; only the top matters.
            Some(Listing::Failed(message)) => {
                if parent.is_empty() {
                    rows.push(info_row(RowContent::Error, depth, message.clone()));
                }
            }
            Some(Listing::Loaded(namespaces)) => {
                if parent.is_empty() && namespaces.is_empty() {
                    rows.push(info_row(RowContent::Info, depth, "No namespaces"));
                }
                for namespace in namespaces {
                    let id = TreeId::Namespace(catalog.to_owned(), namespace.clone());
                    let expanded = self.expanded.contains(&id);
                    let label = namespace.last().cloned().unwrap_or_default();
                    rows.push(Row {
                        content: RowContent::Tree(id),
                        depth,
                        label: label.into(),
                        detail: None,
                        expanded: Some(expanded),
                    });
                    if expanded {
                        self.push_namespace(catalog, namespace, depth + 1, rows, needs);
                    }
                }
            }
        }
        if !parent.is_empty() {
            self.push_tables(catalog, parent, depth, rows, needs);
        }
    }

    fn push_tables(
        &self,
        catalog: &str,
        namespace: &[String],
        depth: usize,
        rows: &mut Vec<Row>,
        needs: &mut Vec<Need>,
    ) {
        let key = (catalog.to_owned(), namespace.to_vec());
        match self.tables.get(&key) {
            None => {
                needs.push(Need::Tables(key.0, key.1));
                rows.push(info_row(RowContent::Info, depth, "Loading…"));
            }
            Some(Listing::Loading) => rows.push(info_row(RowContent::Info, depth, "Loading…")),
            Some(Listing::Failed(message)) => {
                rows.push(info_row(RowContent::Error, depth, message.clone()));
            }
            Some(Listing::Loaded(tables)) => {
                let has_namespaces = matches!(
                    self.namespaces.get(&key),
                    Some(Listing::Loaded(namespaces)) if !namespaces.is_empty()
                );
                if tables.is_empty() && !has_namespaces {
                    rows.push(info_row(RowContent::Info, depth, "No tables"));
                }
                for name in tables {
                    let key = TableKey {
                        catalog: catalog.to_owned(),
                        namespace: namespace.to_vec(),
                        name: name.clone(),
                    };
                    self.push_table(key, depth, rows, needs);
                }
            }
        }
    }

    fn push_table(&self, key: TableKey, depth: usize, rows: &mut Vec<Row>, needs: &mut Vec<Need>) {
        let id = TreeId::Table(key.clone());
        let expanded = self.expanded.contains(&id);
        let detail = match self.loaded.get(&key) {
            Some(Listing::Loaded(table)) => Some(format!("v{}", table.format_version).into()),
            Some(Listing::Loading | Listing::Failed(_)) | None => None,
        };
        rows.push(Row {
            content: RowContent::Tree(id),
            depth,
            label: key.name.clone().into(),
            detail,
            expanded: Some(expanded),
        });
        if !expanded {
            return;
        }
        let depth = depth + 1;
        let table = match self.loaded.get(&key) {
            None => {
                needs.push(Need::Table(key));
                rows.push(info_row(RowContent::Info, depth, "Loading…"));
                return;
            }
            Some(Listing::Loading) => {
                rows.push(info_row(RowContent::Info, depth, "Loading…"));
                return;
            }
            Some(Listing::Failed(message)) => {
                rows.push(info_row(RowContent::Error, depth, message.clone()));
                return;
            }
            Some(Listing::Loaded(table)) => table.clone(),
        };
        rows.push(value_row(
            depth,
            "location",
            Some(table.location.clone()),
            &table.location,
        ));
        self.push_schema(&key, &table, depth, rows);
        self.push_partition(&key, &table, depth, rows);
        self.push_snapshots(&key, &table, depth, rows, needs);
        self.push_properties(&key, &table, depth, rows);
        self.push_metadata_files(&key, &table, depth, rows);
    }

    /// A section row; true when it is open.
    fn push_section(
        &self,
        key: &TableKey,
        section: Section,
        label: String,
        depth: usize,
        rows: &mut Vec<Row>,
    ) -> bool {
        let id = TreeId::Section(key.clone(), section);
        let expanded = self.expanded.contains(&id);
        rows.push(Row {
            content: RowContent::Tree(id),
            depth,
            label: label.into(),
            detail: None,
            expanded: Some(expanded),
        });
        expanded
    }

    fn push_schema(&self, key: &TableKey, table: &TableInfo, depth: usize, rows: &mut Vec<Row>) {
        let label = format!("Schema ({} columns)", table.fields.len());
        if !self.push_section(key, Section::Schema, label, depth, rows) {
            return;
        }
        for field in &table.fields {
            let required = if field.required { " · required" } else { "" };
            rows.push(value_row(
                depth + 1,
                &field.name,
                Some(format!("{}{required}", field.type_name)),
                &field.name,
            ));
        }
    }

    fn push_partition(&self, key: &TableKey, table: &TableInfo, depth: usize, rows: &mut Vec<Row>) {
        if table.partition.is_empty() {
            rows.push(info_row(RowContent::Info, depth, "Unpartitioned"));
            return;
        }
        let transforms: Vec<&str> = table
            .partition
            .iter()
            .map(|field| field.transform.as_str())
            .collect();
        let label = format!("Partitioned by {}", transforms.join(", "));
        if !self.push_section(key, Section::Partition, label, depth, rows) {
            return;
        }
        for field in &table.partition {
            rows.push(value_row(
                depth + 1,
                &field.name,
                Some(field.transform.clone()),
                &field.name,
            ));
        }
    }

    fn push_snapshots(
        &self,
        key: &TableKey,
        table: &TableInfo,
        depth: usize,
        rows: &mut Vec<Row>,
        needs: &mut Vec<Need>,
    ) {
        let label = format!("Snapshots ({})", table.snapshots.len());
        if !self.push_section(key, Section::Snapshots, label, depth, rows) {
            return;
        }
        if table.snapshots.is_empty() {
            rows.push(info_row(RowContent::Info, depth + 1, "No snapshots yet"));
        }
        for snapshot in table.snapshots.iter().rev() {
            self.push_snapshot(
                key,
                snapshot,
                table.current_snapshot,
                depth + 1,
                rows,
                needs,
            );
        }
    }

    fn push_snapshot(
        &self,
        key: &TableKey,
        snapshot: &SnapshotInfo,
        current: Option<i64>,
        depth: usize,
        rows: &mut Vec<Row>,
        needs: &mut Vec<Need>,
    ) {
        let id = TreeId::Snapshot(key.clone(), snapshot.id);
        let expanded = self.expanded.contains(&id);
        let mut detail = vec![snapshot.operation.clone()];
        let change = snapshot.change();
        if !change.is_empty() {
            detail.push(change);
        }
        if current == Some(snapshot.id) {
            detail.push("current".to_owned());
        }
        rows.push(Row {
            content: RowContent::Tree(id),
            depth,
            label: sql_iceberg::timestamp_text(snapshot.timestamp_ms).into(),
            detail: Some(detail.join(" · ").into()),
            expanded: Some(expanded),
        });
        if !expanded {
            return;
        }
        let depth = depth + 1;
        rows.push(value_row(
            depth,
            "id",
            Some(snapshot.id.to_string()),
            &snapshot.id.to_string(),
        ));
        if let Some(parent) = snapshot.parent {
            rows.push(value_row(
                depth,
                "parent",
                Some(parent.to_string()),
                &parent.to_string(),
            ));
        }
        rows.push(value_row(
            depth,
            "manifest list",
            Some(sql_iceberg::file_name(&snapshot.manifest_list).to_owned()),
            &snapshot.manifest_list,
        ));
        let slot = (key.clone(), snapshot.id);
        match self.manifests.get(&slot) {
            None => {
                needs.push(Need::Manifests(slot.0, slot.1));
                rows.push(info_row(RowContent::Info, depth, "Loading manifests…"));
            }
            Some(Listing::Loading) => {
                rows.push(info_row(RowContent::Info, depth, "Loading manifests…"));
            }
            Some(Listing::Failed(message)) => {
                rows.push(info_row(RowContent::Error, depth, message.clone()));
            }
            Some(Listing::Loaded(manifests)) => {
                for manifest in manifests.iter() {
                    self.push_manifest(key, snapshot.id, manifest, depth, rows, needs);
                }
            }
        }
    }

    fn push_manifest(
        &self,
        key: &TableKey,
        snapshot: i64,
        manifest: &ManifestInfo,
        depth: usize,
        rows: &mut Vec<Row>,
        needs: &mut Vec<Need>,
    ) {
        let id = TreeId::Manifest(key.clone(), snapshot, manifest.path.clone());
        let expanded = self.expanded.contains(&id);
        let files = manifest.live_files();
        let plural = if files == 1 { "" } else { "s" };
        let kind = if manifest.content == "deletes" {
            "delete"
        } else {
            "data"
        };
        let mut detail = vec![format!("{files} {kind} file{plural}")];
        if let Some(rows_added) = manifest.added_rows.filter(|rows| *rows > 0) {
            detail.push(format!("+{rows_added} rows"));
        }
        if manifest.added_snapshot != snapshot {
            detail.push("from an earlier snapshot".to_owned());
        }
        rows.push(Row {
            content: RowContent::Tree(id),
            depth,
            label: sql_iceberg::file_name(&manifest.path).to_owned().into(),
            detail: Some(detail.join(" · ").into()),
            expanded: Some(expanded),
        });
        if !expanded {
            return;
        }
        let depth = depth + 1;
        let slot = (key.clone(), snapshot, manifest.path.clone());
        match self.files.get(&slot) {
            None => {
                needs.push(Need::Files(slot.0, slot.1, slot.2));
                rows.push(info_row(RowContent::Info, depth, "Loading files…"));
            }
            Some(Listing::Loading) => {
                rows.push(info_row(RowContent::Info, depth, "Loading files…"));
            }
            Some(Listing::Failed(message)) => {
                rows.push(info_row(RowContent::Error, depth, message.clone()));
            }
            Some(Listing::Loaded(files)) => {
                for file in files.iter() {
                    rows.push(file_row(key, snapshot, file, depth));
                }
            }
        }
    }

    fn push_properties(
        &self,
        key: &TableKey,
        table: &TableInfo,
        depth: usize,
        rows: &mut Vec<Row>,
    ) {
        let label = format!("Properties ({})", table.properties.len());
        if !self.push_section(key, Section::Properties, label, depth, rows) {
            return;
        }
        for (name, value) in &table.properties {
            rows.push(value_row(
                depth + 1,
                name,
                Some(value.clone()),
                &format!("{name}={value}"),
            ));
        }
    }

    fn push_metadata_files(
        &self,
        key: &TableKey,
        table: &TableInfo,
        depth: usize,
        rows: &mut Vec<Row>,
    ) {
        let count = table.metadata_log.len() + usize::from(table.metadata_location.is_some());
        let label = format!("Metadata files ({count})");
        if !self.push_section(key, Section::MetadataFiles, label, depth, rows) {
            return;
        }
        if let Some(current) = &table.metadata_location {
            rows.push(value_row(
                depth + 1,
                sql_iceberg::file_name(current),
                Some("current".to_owned()),
                current,
            ));
        }
        for entry in table.metadata_log.iter().rev() {
            rows.push(value_row(
                depth + 1,
                sql_iceberg::file_name(&entry.file),
                Some(sql_iceberg::timestamp_text(entry.timestamp_ms)),
                &entry.file,
            ));
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
                let is_selected = matches!(&row.content, RowContent::Tree(id) if self.selected.as_ref() == Some(id));
                let text_color = match &row.content {
                    RowContent::Error => cx.theme().danger,
                    RowContent::Info => cx.theme().muted_foreground,
                    RowContent::Tree(_) | RowContent::Value { .. } => cx.theme().foreground,
                };
                let base = h_flex()
                    .id(("iceberg-row", index))
                    .w_full()
                    .h(px(24.))
                    .pl(indent + px(4.))
                    .pr_1()
                    .gap_1()
                    .text_sm()
                    .text_color(text_color)
                    .when(is_selected, |row| row.bg(cx.theme().selection))
                    .hover(|row| row.bg(cx.theme().list_hover))
                    .child(div().w(px(10.)).flex_shrink_0().child(chevron))
                    .child(div().flex_shrink_0().max_w_full().truncate().child(row.label.clone()))
                    .children(row.detail.clone().map(|detail| {
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(detail)
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| this.on_row_click(index, cx)));
                let element = match &row.content {
                    RowContent::Tree(id) => {
                        let id = id.clone();
                        let explorer = explorer.clone();
                        base.context_menu(move |menu, _, cx| {
                            let Some(this) = explorer.upgrade() else {
                                return menu;
                            };
                            this.read(cx).context_menu_for(&id, menu, &explorer)
                        })
                        .into_any_element()
                    }
                    RowContent::Value { copy } => {
                        let copy = copy.clone();
                        base.context_menu(move |menu, _, _| {
                            let copy = copy.clone();
                            menu.item(PopupMenuItem::new("Copy").on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                            }))
                        })
                        .into_any_element()
                    }
                    RowContent::Error => {
                        let message = row.label.to_string();
                        base.context_menu(move |menu, _, _| {
                            let message = message.clone();
                            menu.item(PopupMenuItem::new("Copy Error").on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(message.clone()));
                            }))
                        })
                        .into_any_element()
                    }
                    RowContent::Info => base.into_any_element(),
                };
                Some(element)
            })
            .collect()
    }
}

impl Render for IcebergExplorer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.rows_built_for != Some(self.tree_version) {
            self.rows = self.build_rows();
            self.rows_built_for = Some(self.tree_version);
            self.load_needs(cx);
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
                    .child("ICEBERG CATALOGS")
                    .child(
                        Button::new("add-iceberg-catalog")
                            .label("Add")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|_, _, window, cx| {
                                Self::ask_catalog(None, window, cx);
                            })),
                    ),
            )
            .when(self.catalogs.is_empty(), |sidebar| {
                sidebar.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("Add a REST catalog, such as http://localhost:8181"),
                )
            })
            .child(
                uniform_list(
                    "iceberg-rows",
                    row_count,
                    cx.processor(|this, range: Range<usize>, _, cx| this.render_rows(range, cx)),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .min_h_0(),
            )
    }
}

fn menu_item(
    explorer: &WeakEntity<IcebergExplorer>,
    label: &'static str,
    action: MenuAction,
) -> PopupMenuItem {
    let explorer = explorer.clone();
    PopupMenuItem::new(label).on_click(move |_, window, cx| {
        let updated = explorer.update(cx, |this, cx| action(this, window, cx));
        if let Err(error) = updated {
            tracing::debug!(%error, "Iceberg tree closed before the menu action");
        }
    })
}

fn copy_item(label: &'static str, text: String) -> PopupMenuItem {
    PopupMenuItem::new(label).on_click(move |_, _, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
    })
}

fn info_row(content: RowContent, depth: usize, label: impl Into<SharedString>) -> Row {
    Row {
        content,
        depth,
        label: label.into(),
        detail: None,
        expanded: None,
    }
}

fn value_row(depth: usize, label: &str, detail: Option<String>, copy: &str) -> Row {
    Row {
        content: RowContent::Value {
            copy: copy.to_owned(),
        },
        depth,
        label: label.to_owned().into(),
        detail: detail.map(SharedString::from),
        expanded: None,
    }
}

fn file_row(key: &TableKey, snapshot: i64, file: &FileInfo, depth: usize) -> Row {
    let mut detail = Vec::new();
    if file.content != "data" {
        detail.push(file.content.to_owned());
    }
    if !file.partition.is_empty() {
        detail.push(file.partition.clone());
    }
    detail.push(format!("{} rows", file.record_count));
    detail.push(sql_iceberg::human_size(file.size_bytes));
    if file.status == "deleted" {
        detail.push("removed".to_owned());
    }
    Row {
        content: RowContent::Tree(TreeId::File {
            table: key.clone(),
            snapshot,
            path: file.path.clone(),
            holds_rows: file.content == "data" && file.status != "deleted",
        }),
        depth,
        label: sql_iceberg::file_name(&file.path).to_owned().into(),
        detail: Some(detail.join(" · ").into()),
        expanded: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_locations_become_local_paths() {
        assert_eq!(
            local_path("file:/data/lake/a.parquet"),
            Some(PathBuf::from("/data/lake/a.parquet"))
        );
        assert_eq!(
            local_path("file:///data/lake/a.parquet"),
            Some(PathBuf::from("/data/lake/a.parquet"))
        );
        assert_eq!(
            local_path("/data/lake/a.parquet"),
            Some(PathBuf::from("/data/lake/a.parquet"))
        );
        assert_eq!(local_path("s3://bucket/a.parquet"), None);
    }
}
