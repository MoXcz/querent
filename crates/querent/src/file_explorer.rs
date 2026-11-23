//! Sidebar tree of the open folders, listed a directory at a time and kept in sync with the disk.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, EventEmitter,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px, uniform_list,
};
use notify::Watcher as _;

use crate::folders::{self, Entry, FileOperation};
use crate::query_tab::ConnectionFields;
use crate::session::SavedFolder;
use crate::settings;

const ROW_INDENT: f32 = 12.;
/// Disk events arrive in bursts (a save is several); the tree reloads once per burst.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(200);

pub enum FileExplorerEvent {
    Open {
        path: PathBuf,
        run: bool,
    },
    /// A file or folder went to the trash; open tabs on it keep their text as unsaved.
    Deleted(PathBuf),
    /// A file or folder moved; open tabs follow it.
    Renamed {
        from: PathBuf,
        to: PathBuf,
    },
    FindInFolders(Vec<PathBuf>),
    OpenFolder,
    /// Folders were opened or closed, so the session changed.
    FoldersChanged,
}

impl EventEmitter<FileExplorerEvent> for FileExplorer {}

enum Load<T> {
    Loading,
    Loaded(T),
    Failed(SharedString),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    /// An open folder, at the top of the tree.
    Root,
    Directory,
    File,
}

enum RowContent {
    Entry { path: PathBuf, kind: EntryKind },
    Info,
    Error,
}

struct Row {
    content: RowContent,
    depth: usize,
    label: SharedString,
    expanded: Option<bool>,
}

/// Every file under an open folder, for the filter, and whether the walk stopped early.
type WalkedFiles = Arc<(Vec<PathBuf>, bool)>;

type MenuAction = Box<dyn Fn(&mut FileExplorer, &mut Window, &mut Context<FileExplorer>)>;

pub struct FileExplorer {
    folders: Vec<SavedFolder>,
    filter_input: Entity<InputState>,
    expanded: HashSet<PathBuf>,
    selected: Option<PathBuf>,
    listings: HashMap<PathBuf, Load<Arc<Vec<Entry>>>>,
    /// `None` while the folder is being walked.
    walked: HashMap<PathBuf, Option<WalkedFiles>>,
    /// Bumped per listing and walk, so an answer older than the latest request is dropped.
    requests: u64,
    latest_listing: HashMap<PathBuf, u64>,
    latest_walk: HashMap<PathBuf, u64>,
    rows: Vec<Row>,
    tree_version: u64,
    rows_built_for: Option<(String, u64)>,
    watcher: Option<notify::RecommendedWatcher>,
    watched: HashSet<PathBuf>,
    _watch_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl FileExplorer {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter_input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter files"));
        let subscriptions = vec![
            cx.subscribe(&filter_input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.on_filter_changed(cx);
                }
            }),
        ];
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<notify::Event>();
        let watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
                Ok(event) if !event.kind.is_access() => {
                    if sender.send(event).is_err() {
                        tracing::debug!("folder watcher outlived the file tree");
                    }
                }
                Ok(_) => {}
                Err(error) => tracing::debug!(%error, "folder watcher error"),
            });
        let watcher = match watcher {
            Ok(watcher) => Some(watcher),
            Err(error) => {
                tracing::warn!(%error, "cannot watch folders; the file tree will not follow the disk");
                None
            }
        };
        let watch_task = cx.spawn(async move |this, cx| {
            while let Some(first) = receiver.recv().await {
                cx.background_executor().timer(WATCH_DEBOUNCE).await;
                let mut events = vec![first];
                while let Ok(event) = receiver.try_recv() {
                    events.push(event);
                }
                if this
                    .update(cx, |this, cx| this.apply_disk_changes(&events, cx))
                    .is_err()
                {
                    return;
                }
            }
        });
        Self {
            folders: Vec::new(),
            filter_input,
            expanded: HashSet::new(),
            selected: None,
            listings: HashMap::new(),
            walked: HashMap::new(),
            requests: 0,
            latest_listing: HashMap::new(),
            latest_walk: HashMap::new(),
            rows: Vec::new(),
            tree_version: 0,
            rows_built_for: None,
            watcher,
            watched: HashSet::new(),
            _watch_task: watch_task,
            _subscriptions: subscriptions,
        }
    }

    pub fn saved_folders(&self) -> Vec<SavedFolder> {
        self.folders.clone()
    }

    pub fn folder_paths(&self) -> Vec<PathBuf> {
        self.folders
            .iter()
            .map(|folder| folder.path.clone())
            .collect()
    }

    /// Reopens the folders of the last session, collapsed except for their top level.
    pub fn restore(&mut self, folders: Vec<SavedFolder>, cx: &mut Context<Self>) {
        for folder in folders {
            let path = folder.path.clone();
            if self.folders.iter().any(|open| open.path == path) {
                continue;
            }
            self.folders.push(folder);
            self.expanded.insert(path.clone());
            self.load_expanded_under(&path, cx);
        }
        self.tree_version += 1;
        cx.notify();
    }

    pub fn open_folder(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.folders.iter().any(|folder| folder.path == path) {
            self.folders.push(SavedFolder {
                path: path.clone(),
                connection: None,
                database: String::new(),
            });
            cx.emit(FileExplorerEvent::FoldersChanged);
        }
        self.expanded.insert(path.clone());
        // Lists the folder, and folders left expanded when it was closed.
        self.load_expanded_under(&path, cx);
        self.selected = Some(path);
        self.walk_if_filtering(&[], cx);
        self.tree_version += 1;
        cx.notify();
    }

    fn close_folder(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.folders.retain(|folder| folder.path != path);
        // A nested or enclosing open folder still shows some of these directories.
        let open: Vec<PathBuf> = self.folder_paths();
        self.release_where(|candidate| {
            candidate.starts_with(path) && !open.iter().any(|folder| candidate.starts_with(folder))
        });
        self.walked.remove(path);
        self.latest_walk.remove(path);
        self.tree_version += 1;
        cx.emit(FileExplorerEvent::FoldersChanged);
        cx.notify();
    }

    /// Drops the listings, watches and in-flight listings of the directories `release` picks.
    fn release_where(&mut self, release: impl Fn(&Path) -> bool) {
        let unwatched: Vec<PathBuf> = self
            .watched
            .iter()
            .filter(|watched| release(watched))
            .cloned()
            .collect();
        self.listings.retain(|directory, _| !release(directory));
        self.latest_listing
            .retain(|directory, _| !release(directory));
        for directory in unwatched {
            self.unwatch(&directory);
        }
    }

    /// Keeps what an open folder nested under `path` shows: it is listed on its own.
    fn release_under(&mut self, path: &Path) {
        let nested: Vec<PathBuf> = self
            .folder_paths()
            .into_iter()
            .filter(|folder| folder != path && folder.starts_with(path))
            .collect();
        self.release_where(|candidate| {
            candidate.starts_with(path)
                && !nested.iter().any(|folder| candidate.starts_with(folder))
        });
    }

    /// Lists the expanded directories under `path` that have no listing yet.
    fn load_expanded_under(&mut self, path: &Path, cx: &mut Context<Self>) {
        let missing: Vec<PathBuf> = self
            .expanded
            .iter()
            .filter(|expanded| {
                expanded.starts_with(path)
                    && !self.listings.contains_key(*expanded)
                    && self.is_shown(expanded)
            })
            .cloned()
            .collect();
        for directory in missing {
            self.load_directory(directory, cx);
        }
    }

    /// The connection last used for a file in the folder that holds `path`.
    pub fn fields_for(&self, path: &Path) -> Option<ConnectionFields> {
        let folder = self.folder_of(path)?;
        let connection = folder.connection.clone()?;
        Some(ConnectionFields {
            connection: Some(connection.into()),
            database: folder.database.clone(),
        })
    }

    /// Notes the connection a tab uses on a file, for the next tab opened from its folder.
    pub fn remember_connection(&mut self, file: &Path, fields: &ConnectionFields) {
        let Some(connection) = &fields.connection else {
            return;
        };
        let Some(root) = folders::containing_folder(
            self.folders.iter().map(|folder| folder.path.as_path()),
            file,
        )
        .map(Path::to_path_buf) else {
            return;
        };
        if let Some(folder) = self.folders.iter_mut().find(|folder| folder.path == root) {
            folder.connection = Some(connection.to_string());
            folder.database.clone_from(&fields.database);
        }
    }

    fn folder_of(&self, path: &Path) -> Option<&SavedFolder> {
        let root = folders::containing_folder(
            self.folders.iter().map(|folder| folder.path.as_path()),
            path,
        )?;
        self.folders.iter().find(|folder| folder.path == root)
    }

    fn filter_text(&self, cx: &Context<Self>) -> String {
        self.filter_input.read(cx).value().trim().to_lowercase()
    }

    fn on_filter_changed(&mut self, cx: &mut Context<Self>) {
        if self.filter_text(cx).is_empty() {
            // The next filter walks again, so it sees files added since.
            self.walked.clear();
        } else {
            self.walk_if_filtering(&[], cx);
        }
        cx.notify();
    }

    /// Walks unwalked folders and those holding a path in `changed`; an old walk stays on screen meanwhile.
    fn walk_if_filtering(&mut self, changed: &[PathBuf], cx: &mut Context<Self>) {
        if self.filter_text(cx).is_empty() {
            return;
        }
        let show_all = settings::get(cx).files_show_all;
        let roots: Vec<PathBuf> = self
            .folders
            .iter()
            .map(|folder| folder.path.clone())
            .filter(|root| {
                !self.walked.contains_key(root) || changed.iter().any(|path| path.starts_with(root))
            })
            .collect();
        for root in roots {
            self.walked.entry(root.clone()).or_insert(None);
            let request = self.next_request();
            self.latest_walk.insert(root.clone(), request);
            let walk = cx.background_spawn({
                let root = root.clone();
                async move { folders::all_files(&root, show_all, folders::MAX_WALKED_FILES) }
            });
            cx.spawn(async move |this, cx| {
                let files = walk.await;
                let updated = this.update(cx, |this, cx| {
                    if this.latest_walk.get(&root) == Some(&request)
                        && this.walked.contains_key(&root)
                    {
                        this.walked.insert(root, Some(Arc::new(files)));
                        this.tree_version += 1;
                        cx.notify();
                    }
                });
                if let Err(error) = updated {
                    tracing::debug!(%error, "file tree closed during a folder walk");
                }
            })
            .detach();
        }
        self.tree_version += 1;
    }

    fn next_request(&mut self) -> u64 {
        self.requests += 1;
        self.requests
    }

    /// Lists `directory` off the UI thread; an earlier listing stays on screen meanwhile.
    fn load_directory(&mut self, directory: PathBuf, cx: &mut Context<Self>) {
        self.listings
            .entry(directory.clone())
            .or_insert(Load::Loading);
        let show_all = settings::get(cx).files_show_all;
        let request = self.next_request();
        self.latest_listing.insert(directory.clone(), request);
        let listing = cx.background_spawn({
            let directory = directory.clone();
            async move { folders::list_directory(&directory, show_all) }
        });
        cx.spawn(async move |this, cx| {
            let listing = listing.await;
            let updated = this.update(cx, |this, cx| {
                if this.latest_listing.get(&directory) != Some(&request) {
                    return;
                }
                let is_root = this.folders.iter().any(|folder| folder.path == directory);
                let is_open = this
                    .folders
                    .iter()
                    .any(|folder| directory.starts_with(&folder.path));
                if !is_open {
                    return;
                }
                match listing {
                    Ok(entries) => {
                        this.listings
                            .insert(directory.clone(), Load::Loaded(Arc::new(entries)));
                        this.watch(directory);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound && !is_root => {
                        this.unwatch(&directory);
                        this.listings.remove(&directory);
                        this.expanded.remove(&directory);
                    }
                    Err(error) => {
                        this.unwatch(&directory);
                        this.listings.insert(
                            directory,
                            Load::Failed(if error.kind() == std::io::ErrorKind::NotFound {
                                "Folder not found; close it from its menu".into()
                            } else {
                                format!("Could not read this folder: {error}").into()
                            }),
                        );
                    }
                }
                this.tree_version += 1;
                cx.notify();
            });
            if let Err(error) = updated {
                tracing::debug!(%error, "file tree closed during a listing");
            }
        })
        .detach();
    }

    fn unwatch(&mut self, directory: &Path) {
        if self.watched.remove(directory)
            && let Some(watcher) = &mut self.watcher
            && let Err(error) = watcher.unwatch(directory)
        {
            tracing::debug!(%error, "could not stop watching a folder");
        }
    }

    fn watch(&mut self, directory: PathBuf) {
        if self.watched.contains(&directory) {
            return;
        }
        let Some(watcher) = &mut self.watcher else {
            return;
        };
        match watcher.watch(&directory, notify::RecursiveMode::NonRecursive) {
            Ok(()) => {
                self.watched.insert(directory);
            }
            Err(error) => tracing::debug!(%error, "cannot watch a folder"),
        }
    }

    fn apply_disk_changes(&mut self, events: &[notify::Event], cx: &mut Context<Self>) {
        let mut directories = HashSet::new();
        for event in events {
            // Edits inside a file change neither listings nor the filter's file list.
            if matches!(
                event.kind,
                notify::EventKind::Modify(
                    notify::event::ModifyKind::Data(_) | notify::event::ModifyKind::Metadata(_)
                )
            ) {
                continue;
            }
            if let notify::EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both,
            )) = event.kind
                && let [from, to] = event.paths.as_slice()
            {
                self.follow_rename(from, to, cx);
            }
            for path in &event.paths {
                directories.insert(path.clone());
                if let Some(parent) = path.parent() {
                    directories.insert(parent.to_path_buf());
                }
            }
        }
        self.reload(directories, cx);
    }

    /// Lists again the loaded directories among `directories` and forgets the filter's walk.
    fn reload(&mut self, directories: impl IntoIterator<Item = PathBuf>, cx: &mut Context<Self>) {
        let directories: Vec<PathBuf> = directories.into_iter().collect();
        for directory in &directories {
            if self.listings.contains_key(directory) {
                self.load_directory(directory.clone(), cx);
            }
        }
        self.walk_if_filtering(&directories, cx);
        cx.notify();
    }

    /// Hidden files were shown or hidden: every listing is out of date.
    fn reload_all(&mut self, cx: &mut Context<Self>) {
        // New requests replace older ones, so the old listings stay on screen until answered.
        let loaded: Vec<PathBuf> = self.listings.keys().cloned().collect();
        for directory in loaded {
            if self.is_shown(&directory) {
                self.load_directory(directory, cx);
            } else {
                self.release_under(&directory);
            }
        }
        let roots = self.folder_paths();
        self.walk_if_filtering(&roots, cx);
        self.tree_version += 1;
        cx.notify();
    }

    /// The directory is an open folder or every folder above it is expanded.
    fn is_shown(&self, directory: &Path) -> bool {
        self.folders.iter().any(|folder| folder.path == directory)
            || (self.expanded.contains(directory)
                && directory
                    .parent()
                    .is_some_and(|parent| self.is_shown(parent)))
    }

    fn toggle(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.tree_version += 1;
        if self.expanded.remove(path) {
            // Collapsed folders cost no watches; expanding lists them again.
            self.release_under(path);
        } else {
            self.expanded.insert(path.to_path_buf());
            // Expanded folders inside this one stayed expanded while it was closed.
            self.load_expanded_under(path, cx);
        }
        cx.notify();
    }

    fn on_row_click(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(Row {
            content: RowContent::Entry { path, kind },
            ..
        }) = self.rows.get(index)
        else {
            return;
        };
        let (path, kind) = (path.clone(), *kind);
        self.selected = Some(path.clone());
        self.tree_version += 1;
        match kind {
            EntryKind::Root | EntryKind::Directory => self.toggle(&path, cx),
            EntryKind::File => cx.emit(FileExplorerEvent::Open { path, run: false }),
        }
        cx.notify();
    }

    fn run_operation(operation: FileOperation, window: &mut Window, cx: &mut Context<Self>) {
        let applied = cx.background_spawn({
            let operation = operation.clone();
            async move { operation.apply() }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = applied.await;
            let shown = this.update_in(cx, |this, window, cx| match result {
                Ok(()) => this.after_operation(operation, cx),
                Err(message) => crate::workspace::notify_error(message, window, cx),
            });
            if let Err(error) = shown {
                tracing::debug!(%error, "file tree closed during a file operation");
            }
        })
        .detach();
    }

    /// Moves what the tree knows about `from` to `to`, and tells open tabs.
    fn follow_rename(&mut self, from: &Path, to: &Path, cx: &mut Context<Self>) {
        self.expanded = self
            .expanded
            .drain()
            .map(|path| folders::renamed_path(&path, from, to).unwrap_or(path))
            .collect();
        self.release_under(from);
        self.load_expanded_under(to, cx);
        cx.emit(FileExplorerEvent::Renamed {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        });
    }

    fn after_operation(&mut self, operation: FileOperation, cx: &mut Context<Self>) {
        let parent = |path: &Path| path.parent().map(Path::to_path_buf);
        match operation {
            FileOperation::CreateFile(path) => {
                self.reload(parent(&path), cx);
                self.selected = Some(path.clone());
                cx.emit(FileExplorerEvent::Open { path, run: false });
            }
            FileOperation::CreateFolder(path) => {
                self.reload(parent(&path), cx);
                self.expanded.insert(path.clone());
                self.load_directory(path.clone(), cx);
                self.selected = Some(path);
            }
            FileOperation::Rename { from, to } => {
                self.reload(parent(&from).into_iter().chain(parent(&to)), cx);
                self.selected = Some(to.clone());
                self.follow_rename(&from, &to, cx);
            }
            FileOperation::Delete(path) => {
                self.reload(parent(&path), cx);
                self.expanded
                    .retain(|expanded| !expanded.starts_with(&path));
                self.release_under(&path);
                cx.emit(FileExplorerEvent::Deleted(path));
            }
        }
        self.tree_version += 1;
    }

    /// Asks for a name, then creates or renames on disk.
    fn ask_name(request: NameRequest, window: &mut Window, cx: &mut Context<Self>) {
        let (title, initial) = match &request {
            NameRequest::NewFile(_) => ("New file", String::new()),
            NameRequest::NewFolder(_) => ("New folder", String::new()),
            NameRequest::Rename(path) => (
                "Rename",
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
        };
        let placeholder = match &request {
            NameRequest::NewFile(_) => "report.sql",
            NameRequest::NewFolder(_) | NameRequest::Rename(_) => "",
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(initial)
                .placeholder(placeholder)
        });
        let explorer = cx.weak_entity();
        let focus_input = input.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let input = input.clone();
            let explorer = explorer.clone();
            let request = request.clone();
            dialog
                .title(title)
                .child(Input::new(&input))
                .footer(
                    DialogFooter::new()
                        .child(
                            DialogClose::new()
                                .child(Button::new("cancel").label("Cancel").outline()),
                        )
                        .child(DialogAction::new().child(Button::new("ok").label("OK").primary())),
                )
                .on_ok(move |_, window, cx| {
                    let typed = input.read(cx).value().to_string();
                    let name = match folders::check_name(&typed) {
                        Ok(name) => name.to_owned(),
                        Err(message) => {
                            window.push_notification(message, cx);
                            return false;
                        }
                    };
                    let operation = request.operation(&name);
                    let started =
                        explorer.update(cx, |_, cx| Self::run_operation(operation, window, cx));
                    if let Err(error) = started {
                        tracing::debug!(%error, "file tree closed while the name dialog was open");
                    }
                    true
                })
        });
        window.defer(cx, move |window, cx| {
            focus_input.update(cx, |input, cx| {
                input.focus(window, cx);
                input.select_all(window, cx);
            });
        });
    }

    fn ask_delete(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let explorer = cx.weak_entity();
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        window.open_alert_dialog(cx, move |alert, _, _| {
            let explorer = explorer.clone();
            let path = path.clone();
            alert
                .confirm()
                .title(format!("Delete {name}?"))
                .description("It goes to the trash, where it can be restored.")
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let path = path.clone();
                    let started = explorer.update(cx, |_, cx| {
                        Self::run_operation(FileOperation::Delete(path), window, cx);
                    });
                    if let Err(error) = started {
                        tracing::debug!(%error, "file tree closed while the question showed");
                    }
                    true
                })
        });
    }

    fn relative_to_folder(&self, path: &Path) -> String {
        let root = self
            .folder_of(path)
            .map_or(path, |folder| folder.path.as_path());
        folders::relative_path(root, path)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one menu per entry kind; splitting would scatter them"
    )]
    fn context_menu_for(
        path: &Path,
        kind: EntryKind,
        menu: PopupMenu,
        explorer: &WeakEntity<Self>,
        relative: String,
    ) -> PopupMenu {
        let item = |label: &'static str, action: MenuAction| {
            let explorer = explorer.clone();
            PopupMenuItem::new(label).on_click(move |_, window, cx| {
                let updated = explorer.update(cx, |this, cx| action(this, window, cx));
                if let Err(error) = updated {
                    tracing::debug!(%error, "file tree closed before the menu action");
                }
            })
        };
        let path = path.to_path_buf();
        let with_path =
            |action: fn(&mut Self, PathBuf, &mut Window, &mut Context<Self>)| -> MenuAction {
                let path = path.clone();
                Box::new(move |this, window, cx| action(this, path.clone(), window, cx))
            };
        let copy_path = {
            let text = path.display().to_string();
            item(
                "Copy Path",
                Box::new(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                }),
            )
        };
        let copy_relative = item(
            "Copy Relative Path",
            Box::new(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(relative.clone()));
            }),
        );
        let reveal = item(
            "Reveal in File Manager",
            with_path(|_, path, _, cx| cx.reveal_path(&path)),
        );
        let new_entries = |menu: PopupMenu| {
            menu.item(item(
                "New File",
                with_path(|_, path, window, cx| {
                    Self::ask_name(NameRequest::NewFile(path), window, cx);
                }),
            ))
            .item(item(
                "New Folder",
                with_path(|_, path, window, cx| {
                    Self::ask_name(NameRequest::NewFolder(path), window, cx);
                }),
            ))
            .item(item(
                "Find in Folder",
                with_path(|_, path, _, cx| cx.emit(FileExplorerEvent::FindInFolders(vec![path]))),
            ))
            .separator()
        };
        let rename_delete = |menu: PopupMenu| {
            menu.item(item(
                "Rename",
                with_path(|_, path, window, cx| {
                    Self::ask_name(NameRequest::Rename(path), window, cx);
                }),
            ))
            .item(item(
                "Delete",
                with_path(|_, path, window, cx| Self::ask_delete(path, window, cx)),
            ))
            .separator()
        };
        let refresh = item(
            "Refresh",
            with_path(|this, path, _, cx| this.reload([path], cx)),
        );
        match kind {
            EntryKind::Root => new_entries(menu)
                .item(copy_path)
                .item(reveal)
                .separator()
                .item(refresh)
                .item(item(
                    "Close Folder",
                    with_path(|this, path, _, cx| this.close_folder(&path, cx)),
                )),
            EntryKind::Directory => rename_delete(new_entries(menu))
                .item(copy_path)
                .item(copy_relative)
                .item(reveal)
                .separator()
                .item(refresh),
            EntryKind::File => rename_delete(
                menu.item(item(
                    "Open",
                    with_path(|_, path, _, cx| {
                        cx.emit(FileExplorerEvent::Open { path, run: false });
                    }),
                ))
                .item(item(
                    "Open and Run",
                    with_path(|_, path, _, cx| {
                        cx.emit(FileExplorerEvent::Open { path, run: true });
                    }),
                ))
                .separator(),
            )
            .item(copy_path)
            .item(copy_relative)
            .item(reveal),
        }
    }

    fn build_rows(&self, filter: &str) -> Vec<Row> {
        let mut rows = Vec::new();
        for folder in &self.folders {
            let root = &folder.path;
            let expanded = !filter.is_empty() || self.expanded.contains(root);
            let label = root.file_name().map_or_else(
                || root.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            rows.push(Row {
                content: RowContent::Entry {
                    path: root.clone(),
                    kind: EntryKind::Root,
                },
                depth: 0,
                label: label.into(),
                expanded: Some(expanded),
            });
            if !expanded {
                continue;
            }
            if filter.is_empty() {
                self.push_children(root, 1, &mut rows);
            } else {
                self.push_filtered(root, filter, &mut rows);
            }
        }
        rows
    }

    fn push_children(&self, directory: &Path, depth: usize, rows: &mut Vec<Row>) {
        let entries = match self.listings.get(directory) {
            Some(Load::Loaded(entries)) => entries.clone(),
            Some(Load::Failed(message)) => {
                rows.push(info_row(RowContent::Error, depth, message.clone()));
                return;
            }
            Some(Load::Loading) | None => {
                rows.push(info_row(RowContent::Info, depth, "Loading…"));
                return;
            }
        };
        if entries.is_empty() {
            rows.push(info_row(RowContent::Info, depth, "Empty"));
        }
        for entry in entries.iter() {
            let kind = if entry.is_dir {
                EntryKind::Directory
            } else {
                EntryKind::File
            };
            let expanded = entry.is_dir.then(|| self.expanded.contains(&entry.path));
            rows.push(Row {
                content: RowContent::Entry {
                    path: entry.path.clone(),
                    kind,
                },
                depth,
                label: entry.name.clone().into(),
                expanded,
            });
            if expanded == Some(true) {
                self.push_children(&entry.path, depth + 1, rows);
            }
        }
    }

    fn push_filtered(&self, root: &Path, filter: &str, rows: &mut Vec<Row>) {
        let Some(Some(walked)) = self.walked.get(root).cloned() else {
            rows.push(info_row(RowContent::Info, 1, "Searching…"));
            return;
        };
        let (files, truncated) = walked.as_ref();
        let matching = folders::filter_tree(root, files, filter);
        if matching.is_empty() {
            rows.push(info_row(RowContent::Info, 1, "No matching files"));
        }
        for row in matching {
            let label = row
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let kind = if row.is_dir {
                EntryKind::Directory
            } else {
                EntryKind::File
            };
            rows.push(Row {
                content: RowContent::Entry {
                    path: row.path,
                    kind,
                },
                depth: row.depth,
                label: label.into(),
                expanded: row.is_dir.then_some(true),
            });
        }
        if *truncated {
            let note = format!(
                "Stopped after {} files; filter a smaller folder",
                folders::MAX_WALKED_FILES
            );
            rows.push(info_row(RowContent::Info, 1, note));
        }
    }

    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let explorer = cx.weak_entity();
        range
            .filter_map(|index| {
                let row = self.rows.get(index)?;
                let indent =
                    px(ROW_INDENT * f32::from(u16::try_from(row.depth).unwrap_or(u16::MAX)));
                let chevron = match row.expanded {
                    Some(true) => "▾",
                    Some(false) => "▸",
                    None => " ",
                };
                let (is_selected, text_color) = match &row.content {
                    RowContent::Entry { path, .. } => {
                        (self.selected.as_ref() == Some(path), cx.theme().foreground)
                    }
                    RowContent::Info => (false, cx.theme().muted_foreground),
                    RowContent::Error => (false, cx.theme().danger),
                };
                let base = h_flex()
                    .id(("file-row", index))
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
                    .child(div().truncate().child(row.label.clone()))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.on_row_click(index, cx);
                    }));
                let element = match &row.content {
                    RowContent::Entry { path, kind } => {
                        let (path, kind) = (path.clone(), *kind);
                        let explorer = explorer.clone();
                        let relative = self.relative_to_folder(&path);
                        base.context_menu(move |menu, _, _| {
                            Self::context_menu_for(&path, kind, menu, &explorer, relative.clone())
                        })
                        .into_any_element()
                    }
                    RowContent::Info | RowContent::Error => base.into_any_element(),
                };
                Some(element)
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
enum NameRequest {
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Rename(PathBuf),
}

impl NameRequest {
    fn operation(&self, name: &str) -> FileOperation {
        match self {
            Self::NewFile(directory) => {
                FileOperation::CreateFile(directory.join(folders::new_file_name(name)))
            }
            Self::NewFolder(directory) => FileOperation::CreateFolder(directory.join(name)),
            Self::Rename(path) => FileOperation::Rename {
                from: path.clone(),
                to: path.with_file_name(name),
            },
        }
    }
}

impl Render for FileExplorer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let show_all = settings::get(cx).files_show_all;
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
                    .child("FOLDERS")
                    .child(
                        h_flex()
                            .child(
                                Button::new("files-show-all")
                                    .ghost()
                                    .xsmall()
                                    .label(if show_all {
                                        "All files"
                                    } else {
                                        "Hide ignored"
                                    })
                                    .tooltip("Show or hide hidden and git-ignored files")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        settings::update(cx, |settings| {
                                            settings.files_show_all = !settings.files_show_all;
                                        });
                                        this.reload_all(cx);
                                    })),
                            )
                            .child(
                                Button::new("find-in-folders")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Search)
                                    .tooltip("Find in folders")
                                    .disabled(self.folders.is_empty())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        cx.emit(FileExplorerEvent::FindInFolders(
                                            this.folder_paths(),
                                        ));
                                    })),
                            )
                            .child(
                                Button::new("open-folder")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Plus)
                                    .tooltip("Open a folder")
                                    .on_click(cx.listener(|_, _, _, cx| {
                                        cx.emit(FileExplorerEvent::OpenFolder);
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
            .when(self.folders.is_empty(), |sidebar| {
                sidebar.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("Open a folder with + or drop one on the window"),
                )
            })
            .child(
                uniform_list(
                    "file-rows",
                    row_count,
                    cx.processor(|this, range: Range<usize>, _, cx| this.render_rows(range, cx)),
                )
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
        expanded: None,
    }
}
