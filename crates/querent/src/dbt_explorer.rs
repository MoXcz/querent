//! Sidebar tree of the dbt projects inside the open folders, read from `target/manifest.json`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, EventEmitter,
    Global, InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollStrategy,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    UniformListScrollHandle, WeakEntity, Window, div, prelude::FluentBuilder as _, px,
    uniform_list,
};
use notify::Watcher as _;
use sql_dbt::{Direction, Node, Project, ResourceType, SqlOrigin};

const ROW_INDENT: f32 = 12.;
/// A dbt project often sits a few folders into a repository, such as `dags/dbt/analytics`.
const PROJECT_SEARCH_DEPTH: usize = 4;
/// dbt rewrites `target/` in a burst of files; the project reloads once per burst.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(500);

/// A table or view a dbt node builds or reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    pub database: String,
    pub schema: String,
    pub name: String,
    pub is_view: bool,
}

pub enum DbtEvent {
    OpenFile(PathBuf),
    /// Opens `sql` in a new tab on the project's connection, and runs it when `run`.
    OpenSql {
        project: PathBuf,
        sql: String,
        database: Option<String>,
        run: bool,
    },
    SelectTop {
        project: PathBuf,
        relation: Relation,
    },
    ShowInDatabase {
        project: PathBuf,
        relation: Relation,
    },
    /// Projects were found or lost, or a preview database changed.
    ProjectsChanged,
}

impl EventEmitter<DbtEvent> for DbtExplorer {}

/// Model and source names for completion inside `ref('` and `source('`.
#[derive(Default)]
pub struct DbtNames {
    pub models: BTreeSet<String>,
    pub sources: BTreeMap<String, BTreeSet<String>>,
}

impl Global for DbtNames {}

enum Load {
    Loading,
    Loaded(Arc<Project>),
    Failed(SharedString),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TreeId {
    Project(PathBuf),
    Group(PathBuf, ResourceType),
    Folder(PathBuf, ResourceType, String),
    Node(PathBuf, String),
}

enum RowContent {
    Tree(TreeId),
    /// A node upstream or downstream of the node above it; a click jumps to it.
    Link {
        project: PathBuf,
        unique_id: String,
    },
    Info,
    Error,
}

struct Row {
    content: RowContent,
    depth: usize,
    label: SharedString,
    detail: Option<SharedString>,
    stale: bool,
    expanded: Option<bool>,
}

type MenuAction = Box<dyn Fn(&mut DbtExplorer, &mut Window, &mut Context<DbtExplorer>)>;

pub struct DbtExplorer {
    folders: Vec<PathBuf>,
    projects: Vec<(PathBuf, Load)>,
    /// Database to use in place of the manifest's, per project, for previews.
    preview_databases: BTreeMap<PathBuf, String>,
    /// Per project root, its target folder and the folder being watched for it.
    targets: HashMap<PathBuf, (PathBuf, PathBuf)>,
    filter_input: Entity<InputState>,
    expanded: HashSet<TreeId>,
    selected: Option<TreeId>,
    rows: Vec<Row>,
    tree_version: u64,
    rows_built_for: Option<(String, u64)>,
    scroll: UniformListScrollHandle,
    /// Bumped on every search for projects, so an older answer is dropped.
    search: u64,
    watcher: Option<notify::RecommendedWatcher>,
    _watch_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

pub fn init(cx: &mut App) {
    cx.set_global(DbtNames::default());
}

impl DbtExplorer {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter: name, tag:x or group:x"));
        let subscriptions = vec![
            cx.subscribe(&filter_input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.tree_version += 1;
                    cx.notify();
                }
            }),
        ];
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else {
                return;
            };
            if event.kind.is_access() {
                return;
            }
            for path in event.paths {
                if sender.send(path).is_err() {
                    tracing::debug!("dbt watcher outlived the dbt tree");
                }
            }
        });
        let watcher = match watcher {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                tracing::warn!(%error, "cannot watch dbt projects; reload them by hand");
                None
            }
        };
        let watch_task = cx.spawn(async move |this, cx| {
            while let Some(first) = receiver.recv().await {
                cx.background_executor().timer(RELOAD_DEBOUNCE).await;
                let mut changed = vec![first];
                while let Ok(path) = receiver.try_recv() {
                    changed.push(path);
                }
                let updated = this.update(cx, |this, cx| {
                    let roots: Vec<PathBuf> = this
                        .projects
                        .iter()
                        .map(|(root, _)| root.clone())
                        .filter(|root| changed.iter().any(|path| this.affects(root, path)))
                        .collect();
                    for root in roots {
                        Self::load_project(root, cx);
                    }
                });
                if updated.is_err() {
                    return;
                }
            }
        });
        Self {
            folders: Vec::new(),
            projects: Vec::new(),
            preview_databases: BTreeMap::new(),
            targets: HashMap::new(),
            filter_input,
            expanded: HashSet::new(),
            selected: None,
            rows: Vec::new(),
            tree_version: 0,
            rows_built_for: None,
            scroll: UniformListScrollHandle::new(),
            search: 0,
            watcher,
            _watch_task: watch_task,
            _subscriptions: subscriptions,
        }
    }

    pub fn has_projects(&self) -> bool {
        !self.projects.is_empty()
    }

    pub fn preview_databases(&self) -> BTreeMap<PathBuf, String> {
        self.preview_databases.clone()
    }

    pub fn set_preview_databases(&mut self, databases: BTreeMap<PathBuf, String>) {
        self.preview_databases = databases;
    }

    /// Looks for projects in the open folders again; call it when folders open or close.
    pub fn set_folders(&mut self, folders: Vec<PathBuf>, cx: &mut Context<Self>) {
        if folders == self.folders {
            return;
        }
        self.folders.clone_from(&folders);
        self.search += 1;
        let search = self.search;
        let found =
            cx.background_spawn(
                async move { sql_dbt::find_projects(&folders, PROJECT_SEARCH_DEPTH) },
            );
        cx.spawn(async move |this, cx| {
            let found = found.await;
            let updated = this.update(cx, |this, cx| {
                if this.search == search {
                    this.set_projects(found, cx);
                }
            });
            if let Err(error) = updated {
                tracing::debug!(%error, "dbt tree closed during the project search");
            }
        })
        .detach();
    }

    fn set_projects(&mut self, roots: Vec<PathBuf>, cx: &mut Context<Self>) {
        let known: Vec<PathBuf> = self.projects.iter().map(|(root, _)| root.clone()).collect();
        for gone in known.iter().filter(|root| !roots.contains(root)) {
            self.unwatch(gone);
        }
        self.projects.retain(|(root, _)| roots.contains(root));
        for root in roots {
            if !known.contains(&root) {
                self.expanded.insert(TreeId::Project(root.clone()));
                self.projects.push((root.clone(), Load::Loading));
                Self::load_project(root, cx);
            }
        }
        self.publish_names(cx);
        self.tree_version += 1;
        cx.emit(DbtEvent::ProjectsChanged);
        cx.notify();
    }

    fn load_project(root: PathBuf, cx: &mut Context<Self>) {
        let loading = cx.background_spawn({
            let root = root.clone();
            async move {
                let target = sql_dbt::target_dir(&root);
                // Until dbt creates the target folder, its creation shows in the root.
                let watched = if target.is_dir() {
                    target.clone()
                } else {
                    root.clone()
                };
                (sql_dbt::load(&root), target, watched)
            }
        });
        cx.spawn(async move |this, cx| {
            let (loaded, target, watched) = loading.await;
            let updated = this.update(cx, |this, cx| {
                let Some(slot) = this.projects.iter_mut().find(|(known, _)| *known == root) else {
                    return;
                };
                slot.1 = match loaded {
                    Ok(project) => Load::Loaded(Arc::new(project)),
                    Err(error) => Load::Failed(error.to_string().into()),
                };
                this.watch(root, target, watched);
                this.publish_names(cx);
                this.tree_version += 1;
                cx.notify();
            });
            if let Err(error) = updated {
                tracing::debug!(%error, "dbt tree closed while a project loaded");
            }
        })
        .detach();
    }

    /// Watches `folder` for the project at `root`, in place of what was watched for it before.
    fn watch(&mut self, root: PathBuf, target: PathBuf, folder: PathBuf) {
        // dbt may have created the target folder since `folder` was picked.
        let folder = if folder != target && target.is_dir() {
            target.clone()
        } else {
            folder
        };
        if self
            .targets
            .get(&root)
            .is_some_and(|(_, watched)| *watched == folder)
        {
            return;
        }
        self.unwatch(&root);
        let Some(watcher) = &mut self.watcher else {
            return;
        };
        if let Err(error) = watcher.watch(&folder, notify::RecursiveMode::NonRecursive) {
            tracing::debug!(%error, "cannot watch a dbt target folder");
        }
        self.targets.insert(root, (target, folder));
    }

    fn unwatch(&mut self, root: &Path) {
        let Some((_, folder)) = self.targets.remove(root) else {
            return;
        };
        let Some(watcher) = &mut self.watcher else {
            return;
        };
        // A deleted folder has already lost its watch; nothing to report then.
        if let Err(error) = watcher.unwatch(&folder) {
            tracing::trace!(%error, "dbt folder was not watched");
        }
    }

    /// Whether a change at `path` calls for reloading the project at `root`: its
    /// manifest or catalog, or its target folder (or a folder above it) appearing or going.
    fn affects(&self, root: &Path, path: &Path) -> bool {
        let Some((target, _)) = self.targets.get(root) else {
            return false;
        };
        let is_artifact = path
            .file_name()
            .is_some_and(|name| name == "manifest.json" || name == "catalog.json");
        (is_artifact && path.parent() == Some(target.as_path())) || target.starts_with(path)
    }

    fn publish_names(&self, cx: &mut App) {
        let mut names = DbtNames::default();
        for project in self.loaded_projects() {
            for node in project.manifest.nodes.values() {
                match node.resource_type {
                    ResourceType::Model | ResourceType::Seed | ResourceType::Snapshot => {
                        names.models.insert(node.name.clone());
                    }
                    ResourceType::Source => {
                        if let Some(group) = &node.source_name {
                            names
                                .sources
                                .entry(group.clone())
                                .or_default()
                                .insert(node.name.clone());
                        }
                    }
                    ResourceType::Test
                    | ResourceType::Analysis
                    | ResourceType::Macro
                    | ResourceType::Exposure
                    | ResourceType::Other => {}
                }
            }
        }
        cx.set_global(names);
    }

    fn loaded_projects(&self) -> impl Iterator<Item = &Arc<Project>> {
        self.projects.iter().filter_map(|(_, load)| match load {
            Load::Loaded(project) => Some(project),
            Load::Loading | Load::Failed(_) => None,
        })
    }

    fn project(&self, root: &Path) -> Option<Arc<Project>> {
        self.loaded_projects()
            .find(|project| project.root == root)
            .cloned()
    }

    /// The database to query in place of `database` for this project.
    fn preview_database(&self, root: &Path, database: &str) -> String {
        self.preview_databases
            .get(root)
            .filter(|chosen| !chosen.is_empty())
            .cloned()
            .unwrap_or_else(|| database.to_owned())
    }

    fn relation_of(&self, project: &Project, node: &Node) -> Option<Relation> {
        let database = node.database.as_deref()?;
        Some(Relation {
            database: self.preview_database(&project.root, database),
            schema: node.schema.clone()?,
            name: node.relation.clone()?,
            is_view: node.is_view(),
        })
    }

    fn filter_text(&self, cx: &App) -> String {
        self.filter_input.read(cx).value().trim().to_lowercase()
    }

    fn toggle(&mut self, id: &TreeId, cx: &mut Context<Self>) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id.clone());
        }
        self.tree_version += 1;
        cx.notify();
    }

    fn on_row_click(
        &mut self,
        index: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.rows.get(index) else {
            return;
        };
        match &row.content {
            RowContent::Tree(id) => {
                let id = id.clone();
                self.selected = Some(id.clone());
                if let TreeId::Node(root, unique_id) = &id
                    && event.click_count() == 2
                {
                    // The double click's first click expanded or collapsed the node.
                    if row.expanded.is_some() {
                        self.toggle(&id, cx);
                    }
                    self.open_file(root, unique_id, cx);
                    return;
                }
                if row.expanded.is_some() {
                    self.toggle(&id, cx);
                }
                self.tree_version += 1;
                cx.notify();
            }
            RowContent::Link { project, unique_id } => {
                let (project, unique_id) = (project.clone(), unique_id.clone());
                self.reveal(&project, &unique_id, window, cx);
            }
            RowContent::Info | RowContent::Error => {}
        }
    }

    /// Opens the folders that lead to a node, selects it and scrolls to it.
    fn reveal(
        &mut self,
        root: &Path,
        unique_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.project(root) else {
            return;
        };
        let Some(node) = project.manifest.nodes.get(unique_id) else {
            return;
        };
        if node.package_name != project.name {
            window.push_notification(
                format!(
                    "{} comes from the installed package {}, which this view does not list.",
                    node.display_name(),
                    node.package_name
                ),
                cx,
            );
            return;
        }
        let kind = node.resource_type;
        self.filter_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.expanded.insert(TreeId::Project(root.to_path_buf()));
        self.expanded
            .insert(TreeId::Group(root.to_path_buf(), kind));
        let folder = sql_dbt::folder_of(node);
        // Every folder on the way down, `marts`, `marts/finanzas`, opens too.
        let parts: Vec<&str> = folder.split('/').filter(|part| !part.is_empty()).collect();
        for end in 1..=parts.len() {
            let path = parts.get(..end).unwrap_or_default().join("/");
            self.expanded
                .insert(TreeId::Folder(root.to_path_buf(), kind, path));
        }
        let id = TreeId::Node(root.to_path_buf(), unique_id.to_owned());
        self.selected = Some(id.clone());
        self.tree_version += 1;
        let filter = self.filter_text(cx);
        self.rows = self.build_rows(&filter);
        self.rows_built_for = Some((filter, self.tree_version));
        if let Some(index) = self
            .rows
            .iter()
            .position(|row| matches!(&row.content, RowContent::Tree(row_id) if *row_id == id))
        {
            self.scroll.scroll_to_item(index, ScrollStrategy::Center);
        }
        cx.notify();
    }

    fn open_file(&mut self, root: &Path, unique_id: &str, cx: &mut Context<Self>) {
        let Some(project) = self.project(root) else {
            return;
        };
        if let Some(node) = project.manifest.nodes.get(unique_id) {
            cx.emit(DbtEvent::OpenFile(project.file_of(node)));
        }
    }

    /// Opens the node's SQL, compiled or rendered, in a new tab, and runs it for a preview.
    fn open_sql(
        &mut self,
        root: &Path,
        unique_id: &str,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.project(root) else {
            return;
        };
        let unique_id = unique_id.to_owned();
        let preview_database = self.preview_databases.get(root).cloned();
        let built = cx.background_spawn(async move {
            let node = project.manifest.nodes.get(&unique_id)?;
            let preview = project.preview_sql(node);
            let database = node.database.clone();
            Some((
                preview,
                database,
                node.original_file_path.clone(),
                project.root.clone(),
            ))
        });
        cx.spawn_in(window, async move |this, cx| {
            let Some((preview, database, file, root)) = built.await else {
                return;
            };
            let shown = this.update_in(cx, |_, window, cx| match preview {
                Ok(preview) => {
                    let (sql, database) = match (&database, &preview_database) {
                        (Some(from), Some(to)) if !to.is_empty() => (
                            sql_dbt::swap_database(&preview.sql, from, to),
                            Some(to.clone()),
                        ),
                        _ => (preview.sql.clone(), database.clone()),
                    };
                    let origin = match preview.origin {
                        SqlOrigin::Compiled => "compiled",
                        SqlOrigin::Rendered => "ref() and source() filled in from the manifest",
                    };
                    let stale = if preview.stale {
                        "; the model changed since dbt last ran, so this may be out of date"
                    } else {
                        ""
                    };
                    cx.emit(DbtEvent::OpenSql {
                        project: root,
                        sql: format!("-- {file} ({origin}{stale})\n{sql}\n"),
                        database,
                        run,
                    });
                }
                Err(error) => {
                    crate::workspace::notify_error(format!("{file}: {error}."), window, cx);
                }
            });
            if let Err(error) = shown {
                tracing::debug!(%error, "dbt tree closed while the SQL was read");
            }
        })
        .detach();
    }

    fn emit_relation(
        &mut self,
        root: &Path,
        unique_id: &str,
        show: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.project(root) else {
            return;
        };
        let Some(node) = project.manifest.nodes.get(unique_id) else {
            return;
        };
        let Some(relation) = self.relation_of(&project, node) else {
            window.push_notification(
                format!(
                    "{} has no table or view in the manifest.",
                    node.display_name()
                ),
                cx,
            );
            return;
        };
        let project = root.to_path_buf();
        cx.emit(if show {
            DbtEvent::ShowInDatabase { project, relation }
        } else {
            DbtEvent::SelectTop { project, relation }
        });
    }

    fn ask_preview_database(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .preview_databases
            .get(&root)
            .cloned()
            .unwrap_or_default();
        let manifest_database = self
            .project(&root)
            .and_then(|project| {
                project
                    .nodes_of(ResourceType::Model)
                    .find_map(|node| node.database.clone())
            })
            .unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(current)
                .placeholder(format!("{manifest_database} (from the manifest)"))
        });
        let explorer = cx.weak_entity();
        let focus = input.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let input = input.clone();
            let explorer = explorer.clone();
            let root = root.clone();
            dialog
                .title("Preview database")
                .child(
                    v_flex()
                        .gap_2()
                        .child(div().text_sm().child(
                            "Previews and Select Top read this database instead of the manifest's. Leave it empty to use the manifest's.",
                        ))
                        .child(Input::new(&input)),
                )
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().child(Button::new("cancel").label("Cancel").outline()))
                        .child(DialogAction::new().child(Button::new("ok").label("OK").primary())),
                )
                .on_ok(move |_, _, cx| {
                    let chosen = input.read(cx).value().trim().to_owned();
                    let root = root.clone();
                    let updated = explorer.update(cx, |this, cx| {
                        if chosen.is_empty() {
                            this.preview_databases.remove(&root);
                        } else {
                            this.preview_databases.insert(root, chosen);
                        }
                        this.tree_version += 1;
                        cx.emit(DbtEvent::ProjectsChanged);
                        cx.notify();
                    });
                    if let Err(error) = updated {
                        tracing::debug!(%error, "dbt tree closed while the dialog was open");
                    }
                    true
                })
        });
        window.defer(cx, move |window, cx| {
            focus.update(cx, |input, cx| input.focus(window, cx));
        });
    }

    fn context_menu_for(
        id: &TreeId,
        node: Option<&Node>,
        menu: PopupMenu,
        explorer: &WeakEntity<Self>,
    ) -> PopupMenu {
        let item = |label: &'static str, action: MenuAction| {
            let explorer = explorer.clone();
            PopupMenuItem::new(label).on_click(move |_, window, cx| {
                let updated = explorer.update(cx, |this, cx| action(this, window, cx));
                if let Err(error) = updated {
                    tracing::debug!(%error, "dbt tree closed before the menu action");
                }
            })
        };
        let copy = |label: &'static str, text: String| {
            PopupMenuItem::new(label).on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            })
        };
        match id {
            TreeId::Project(root) => {
                let reload = root.clone();
                let database = root.clone();
                menu.item(item(
                    "Preview Database…",
                    Box::new(move |this, window, cx| {
                        this.ask_preview_database(database.clone(), window, cx);
                    }),
                ))
                .separator()
                .item(item(
                    "Reload",
                    Box::new(move |_, _, cx| Self::load_project(reload.clone(), cx)),
                ))
                .item(copy("Copy Path", root.display().to_string()))
            }
            TreeId::Node(root, unique_id) => {
                let Some(node) = node else {
                    return menu;
                };
                let with_node = |action: fn(
                    &mut Self,
                    &Path,
                    &str,
                    &mut Window,
                    &mut Context<Self>,
                )|
                 -> MenuAction {
                    let (root, unique_id) = (root.clone(), unique_id.clone());
                    Box::new(move |this, window, cx| action(this, &root, &unique_id, window, cx))
                };
                let mut menu = menu.item(item(
                    "Open File",
                    with_node(|this, root, id, _, cx| this.open_file(root, id, cx)),
                ));
                let has_sql = matches!(
                    node.resource_type,
                    ResourceType::Model
                        | ResourceType::Snapshot
                        | ResourceType::Analysis
                        | ResourceType::Test
                ) && Project::has_sql_file(node);
                if has_sql {
                    menu = menu
                        .item(item(
                            "Open Compiled SQL",
                            with_node(|this, root, id, window, cx| {
                                this.open_sql(root, id, false, window, cx);
                            }),
                        ))
                        .item(item(
                            "Preview",
                            with_node(|this, root, id, window, cx| {
                                this.open_sql(root, id, true, window, cx);
                            }),
                        ));
                }
                if node.resource_type.is_relation() {
                    menu = menu
                        .separator()
                        .item(item(
                            "Select Top 1000 Rows",
                            with_node(|this, root, id, window, cx| {
                                this.emit_relation(root, id, false, window, cx);
                            }),
                        ))
                        .item(item(
                            "Show in Database",
                            with_node(|this, root, id, window, cx| {
                                this.emit_relation(root, id, true, window, cx);
                            }),
                        ));
                }
                menu = menu
                    .separator()
                    .item(copy("Copy Name", node.display_name()));
                if let Some(relation) = &node.relation_name {
                    menu = menu.item(copy("Copy Relation Name", relation.clone()));
                }
                menu
            }
            TreeId::Group(..) | TreeId::Folder(..) => menu,
        }
    }

    fn build_rows(&self, filter: &str) -> Vec<Row> {
        let mut rows = Vec::new();
        for (root, load) in &self.projects {
            let project_id = TreeId::Project(root.clone());
            let label = match load {
                Load::Loaded(project) => project.name.clone(),
                Load::Loading | Load::Failed(_) => root
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            };
            let detail = self
                .preview_databases
                .get(root)
                .map(|database| SharedString::from(format!("preview in {database}")));
            let expanded = !filter.is_empty() || self.expanded.contains(&project_id);
            rows.push(Row {
                content: RowContent::Tree(project_id),
                depth: 0,
                label: label.into(),
                detail,
                stale: false,
                expanded: Some(expanded),
            });
            if !expanded {
                continue;
            }
            match load {
                Load::Loading => rows.push(info_row(RowContent::Info, 1, "Loading…")),
                Load::Failed(message) => rows.push(info_row(RowContent::Error, 1, message.clone())),
                Load::Loaded(project) => self.push_groups(project, filter, &mut rows),
            }
        }
        rows
    }

    fn push_groups(&self, project: &Project, filter: &str, rows: &mut Vec<Row>) {
        let root = &project.root;
        for kind in ResourceType::SHOWN {
            let mut matching: Vec<&Node> = project
                .nodes_of(kind)
                .filter(|node| sql_dbt::matches_filter(node, filter))
                .collect();
            if matching.is_empty() {
                continue;
            }
            matching.sort_by(|left, right| {
                sql_dbt::folder_of(left)
                    .cmp(&sql_dbt::folder_of(right))
                    .then_with(|| left.name.cmp(&right.name))
            });
            let group = TreeId::Group(root.clone(), kind);
            let expanded = !filter.is_empty() || self.expanded.contains(&group);
            rows.push(Row {
                content: RowContent::Tree(group),
                depth: 1,
                label: format!("{} ({})", kind.plural_label(), matching.len()).into(),
                detail: None,
                stale: false,
                expanded: Some(expanded),
            });
            if !expanded {
                continue;
            }
            // Keyed by parts, so `a/b` sorts right after `a`, before `a-x`.
            let mut by_folder: BTreeMap<Vec<String>, Vec<&Node>> = BTreeMap::new();
            for node in matching {
                let parts = sql_dbt::folder_of(node)
                    .split('/')
                    .filter(|part| !part.is_empty())
                    .map(str::to_owned)
                    .collect();
                by_folder.entry(parts).or_default().push(node);
            }
            self.push_folder_tree(project, kind, &by_folder, filter, rows);
        }
    }

    /// Nested folders, `marts` then `marts/finanzas`, each with its nodes.
    fn push_folder_tree(
        &self,
        project: &Project,
        kind: ResourceType,
        by_folder: &BTreeMap<Vec<String>, Vec<&Node>>,
        filter: &str,
        rows: &mut Vec<Row>,
    ) {
        let root = &project.root;
        let mut open_parts: Vec<String> = Vec::new();
        // A folder row is shown only when the folders above it are open.
        let mut hidden_below: Option<usize> = None;
        for (parts, nodes) in by_folder {
            let shared = open_parts
                .iter()
                .zip(parts)
                .take_while(|(left, right)| left == right)
                .count();
            open_parts.truncate(shared);
            if hidden_below.is_some_and(|depth| depth > shared) {
                hidden_below = None;
            }
            for (index, part) in parts.iter().enumerate().skip(shared) {
                open_parts.push(part.clone());
                if hidden_below.is_some() {
                    continue;
                }
                let path = parts.get(..=index).unwrap_or_default().join("/");
                let id = TreeId::Folder(root.clone(), kind, path);
                let expanded = !filter.is_empty() || self.expanded.contains(&id);
                rows.push(Row {
                    content: RowContent::Tree(id),
                    depth: 2 + index,
                    label: format!("{part}/").into(),
                    detail: None,
                    stale: false,
                    expanded: Some(expanded),
                });
                if !expanded {
                    hidden_below = Some(index + 1);
                }
            }
            if hidden_below.is_some() {
                continue;
            }
            for node in nodes {
                self.push_node(project, node, 2 + parts.len(), rows);
            }
        }
    }

    fn push_node(&self, project: &Project, node: &Node, depth: usize, rows: &mut Vec<Row>) {
        let id = TreeId::Node(project.root.clone(), node.unique_id.clone());
        let has_lineage = node.resource_type != ResourceType::Macro;
        let expanded = has_lineage && self.expanded.contains(&id);
        let mut detail: Vec<String> = Vec::new();
        if let Some(materialized) = &node.materialized
            && node.resource_type == ResourceType::Model
        {
            detail.push(materialized.clone());
        }
        detail.extend(node.tags.iter().cloned());
        let stale = project.stale.contains(&node.unique_id);
        rows.push(Row {
            content: RowContent::Tree(id),
            depth,
            label: node.display_name().into(),
            detail: (!detail.is_empty()).then(|| detail.join(" · ").into()),
            stale,
            expanded: has_lineage.then_some(expanded),
        });
        if !expanded {
            return;
        }
        for (direction, arrow) in [(Direction::Upstream, "↑"), (Direction::Downstream, "↓")] {
            for linked in project.lineage(&node.unique_id, direction) {
                rows.push(Row {
                    content: RowContent::Link {
                        project: project.root.clone(),
                        unique_id: linked.unique_id.clone(),
                    },
                    depth: depth + 1,
                    label: format!("{arrow} {}", linked.display_name()).into(),
                    detail: Some(linked.resource_type.singular_label().into()),
                    stale: false,
                    expanded: None,
                });
            }
        }
        if project
            .lineage(&node.unique_id, Direction::Upstream)
            .is_empty()
            && project
                .lineage(&node.unique_id, Direction::Downstream)
                .is_empty()
        {
            rows.push(info_row(
                RowContent::Info,
                depth + 1,
                "Nothing upstream or downstream",
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
                    RowContent::Tree(_) | RowContent::Link { .. } => cx.theme().foreground,
                };
                let base = h_flex()
                    .id(("dbt-row", index))
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
                    .when(row.stale, |line| {
                        line.child(
                            div()
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(cx.theme().warning)
                                .child("changed")
                                .id(("dbt-stale", index))
                                .tooltip(|window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new(
                                        "Edited since dbt last wrote the manifest; its compiled SQL may be old.",
                                    )
                                    .build(window, cx)
                                }),
                        )
                    })
                    .children(row.detail.clone().map(|detail| {
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(detail)
                    }))
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        this.on_row_click(index, event, window, cx);
                    }));
                let element = match &row.content {
                    RowContent::Tree(id) => {
                        let id = id.clone();
                        let node = match &id {
                            TreeId::Node(root, unique_id) => self
                                .project(root)
                                .and_then(|project| project.manifest.nodes.get(unique_id).cloned()),
                            TreeId::Project(_) | TreeId::Group(..) | TreeId::Folder(..) => None,
                        };
                        let explorer = explorer.clone();
                        base.context_menu(move |menu, _, _| {
                            Self::context_menu_for(&id, node.as_ref(), menu, &explorer)
                        })
                        .into_any_element()
                    }
                    RowContent::Link { .. } | RowContent::Info | RowContent::Error => base.into_any_element(),
                };
                Some(element)
            })
            .collect()
    }
}

impl Render for DbtExplorer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let built_for = (self.filter_text(cx), self.tree_version);
        if self.rows_built_for.as_ref() != Some(&built_for) {
            self.rows = self.build_rows(&built_for.0);
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
                    .child("DBT PROJECTS"),
            )
            .child(
                div()
                    .px_2()
                    .pb_1()
                    .child(Input::new(&self.filter_input).xsmall()),
            )
            .when(self.projects.is_empty(), |sidebar| {
                sidebar.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("Open a folder that holds a dbt_project.yml"),
                )
            })
            .child(
                uniform_list(
                    "dbt-rows",
                    row_count,
                    cx.processor(|this, range: Range<usize>, _, cx| this.render_rows(range, cx)),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .min_h_0(),
            )
    }
}

fn info_row(content: RowContent, depth: usize, label: impl Into<SharedString>) -> Row {
    Row {
        content,
        depth,
        label: label.into(),
        detail: None,
        stale: false,
        expanded: None,
    }
}

/// Completion items for a name typed inside `ref('` or `source('`, if the cursor is in one.
pub fn completion_names(
    text: &str,
    offset: usize,
    cx: &App,
) -> Option<(usize, Vec<(String, &'static str)>)> {
    let (context, start) = sql_dbt::name_context(text, offset)?;
    let names = cx.try_global::<DbtNames>()?;
    let items: Vec<(String, &'static str)> = match context {
        sql_dbt::NameContext::Model => names
            .models
            .iter()
            .map(|name| (name.clone(), "dbt model"))
            .collect(),
        sql_dbt::NameContext::SourceGroup => names
            .sources
            .keys()
            .map(|name| (name.clone(), "dbt source"))
            .collect(),
        sql_dbt::NameContext::SourceTable(group) => names
            .sources
            .get(&group)
            .into_iter()
            .flatten()
            .map(|name| (name.clone(), "dbt source table"))
            .collect(),
    };
    Some((start, items))
}
