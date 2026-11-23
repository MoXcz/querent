use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::command::{Command, CommandItem, CommandState};
use gpui_kit::component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{
    ActiveTheme as _, IconName, Sizable as _, WindowExt as _, h_flex, h_resizable, resizable_panel,
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, ExternalPaths, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, Render, SharedString, Styled as _,
    Subscription, Task, Window, actions, div, px,
};

use crate::command_palette::{self, OpenCommandPalette, OpenSettings};
use crate::connection_form::ConnectionForm;
use crate::connections::{self, SavedConnection};
use crate::file_explorer::{FileExplorer, FileExplorerEvent};
use crate::find_in_folder::{FindEvent, FindInFolder};
use crate::folders;
use crate::history::{self, HistoryEntry};
use crate::lsp_servers::{self, LspServers, ServerStatus};
use crate::object_explorer::{ExplorerEvent, ObjectExplorer};
use crate::query_tab::{ConnectionFields, FileText, QueryTab, read_sql_file};
use crate::release_notes::LastStart;
use crate::session::{self, SavedTab, Session, SidebarView};
use crate::tokio_bridge;

pub const WORKSPACE_CONTEXT: &str = "Workspace";

actions!(
    querent,
    [
        /// Open a new query tab with the current tab's connection.
        NewTab,
        /// Close the current tab and cancel its running query.
        CloseTab,
        /// Switch to the next tab.
        NextTab,
        /// Switch to the previous tab.
        PreviousTab,
        /// Open the dialog to add a connection.
        AddConnection,
        /// Show earlier runs on the current tab's connection.
        ShowHistory,
        /// Open `.sql` files, each in a tab.
        OpenFile,
        /// Bring back the tab closed last.
        ReopenClosedTab,
        /// Open a folder in the sidebar's file tree.
        OpenFolder,
        /// Search the text of the files in the open folders.
        FindInFolders
    ]
);

/// What to do once a file is open in a tab.
#[derive(Debug, Clone, Copy, Default)]
struct FileOpening {
    run: bool,
    /// 1-based line to put the cursor on.
    line: Option<usize>,
}

struct OpenTab {
    view: Entity<QueryTab>,
    /// Running when last observed, so the start of a run is seen once.
    was_running: bool,
    _observation: Subscription,
}

pub struct Workspace {
    tabs: Vec<OpenTab>,
    active_index: usize,
    next_tab_number: usize,
    explorer: Entity<ObjectExplorer>,
    _explorer_subscription: Subscription,
    file_explorer: Entity<FileExplorer>,
    _file_explorer_subscription: Subscription,
    sidebar: SidebarView,
    /// The open Find in Folder dialog's events.
    find_subscription: Option<Subscription>,
    open_folder_task: Option<Task<()>>,
    _servers_observation: Subscription,
    _quit_subscription: Subscription,
    /// Pending session write; tabs notify often, so writes are batched.
    session_save: Option<Task<()>>,
    last_saved: Option<Session>,
    /// Tabs closed in this run, the latest last, for `ReopenClosedTab`.
    closed_tabs: Vec<SavedTab>,
    open_file_task: Option<Task<()>>,
    _activation: Subscription,
    /// The user agreed to close the window with transactions open.
    close_confirmed: bool,
    _sign_in_prompts: Option<Task<()>>,
    _startup_notices: Task<()>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let explorer = cx.new(|cx| ObjectExplorer::new(window, cx));
        let explorer_subscription = cx.subscribe_in(&explorer, window, Self::on_explorer_event);
        let file_explorer = cx.new(|cx| FileExplorer::new(window, cx));
        let file_explorer_subscription =
            cx.subscribe_in(&file_explorer, window, Self::on_file_explorer_event);
        let mut workspace = Self {
            tabs: Vec::new(),
            active_index: 0,
            next_tab_number: 1,
            explorer,
            _explorer_subscription: explorer_subscription,
            file_explorer,
            _file_explorer_subscription: file_explorer_subscription,
            sidebar: SidebarView::default(),
            find_subscription: None,
            open_folder_task: None,
            // The status line shows the active tab's language server.
            _servers_observation: cx.observe_global::<LspServers>(|_, cx| cx.notify()),
            _quit_subscription: cx.on_app_quit(|workspace, cx| {
                workspace.prepare_to_close(cx);
                async {}
            }),
            session_save: None,
            last_saved: None,
            closed_tabs: Vec::new(),
            open_file_task: None,
            // Coming back from another program is when a file may have changed.
            _activation: cx.observe_window_activation(window, |workspace, window, cx| {
                if window.is_window_active()
                    && let Some(tab) = workspace.active_tab().cloned()
                {
                    tab.update(cx, |tab, cx| tab.check_file_on_disk(window, cx));
                }
            }),
            close_confirmed: false,
            _sign_in_prompts: crate::sign_in::show_prompts(window, cx),
            _startup_notices: Self::show_unsaved_config(window, cx),
        };
        crate::updater::check_on_start(window, cx);
        // Closing the last window may drop the workspace before quit hooks run.
        let closing = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            closing.upgrade().is_none_or(|workspace| {
                workspace.update(cx, |workspace, cx| {
                    workspace.confirm_window_close(window, cx)
                })
            })
        });
        let saved = session::load(cx);
        let last_start = match &saved {
            None => LastStart::None,
            Some(Session {
                last_version: None, ..
            }) => LastStart::Unrecorded,
            Some(Session {
                last_version: Some(version),
                ..
            }) => LastStart::Version(version),
        };
        if let Some(notes) = crate::release_notes::notes_after_update(last_start) {
            Self::show_release_notes_soon(notes, window, cx);
        }
        if let Some(saved) = saved {
            let updated =
                saved.last_version.as_deref() != Some(crate::release_notes::CURRENT_VERSION);
            workspace.restore(saved, window, cx);
            if updated {
                // Record the new version now, so the notes show once even after a crash.
                workspace.last_saved = None;
                workspace.schedule_session_save(cx);
            }
            return workspace;
        }
        let first_connection = connections::names(cx).into_iter().next();
        let fields = ConnectionFields {
            connection: first_connection.clone(),
            database: String::new(),
        };
        workspace.open_tab(fields, window, cx);
        if let (Some(name), Some(tab)) = (first_connection, workspace.active_tab().cloned()) {
            tab.update(cx, |tab, cx| tab.use_connection(&name, window, cx));
        }
        workspace
    }

    fn show_release_notes_soon(notes: String, window: &mut Window, cx: &mut Context<Self>) {
        // Shown from a task: the window has no dialog layer until it is open.
        cx.spawn_in(window, async move |_, cx| {
            let shown = cx.update(|window, cx| crate::release_notes::show(Some(notes), window, cx));
            if let Err(error) = shown {
                tracing::debug!(%error, "window closed before the release notes showed");
            }
        })
        .detach();
    }

    /// Says once which config file is not being saved, so changes are not lost unnoticed.
    fn show_unsaved_config(window: &mut Window, cx: &mut Context<Self>) -> Task<()> {
        let unsaved: Vec<&str> = [
            ("settings.toml", crate::settings::are_saved(cx)),
            ("connections.toml", connections::are_saved(cx)),
        ]
        .into_iter()
        .filter_map(|(file, saved)| (!saved).then_some(file))
        .collect();
        // Shown from a task: the window has no notification layer until it is open.
        cx.spawn_in(window, async move |_, cx| {
            for file in unsaved {
                let shown = cx.update(|window, cx| {
                    window.push_notification(
                        format!(
                            "{file} could not be read or written, so changes are not saved in this session. Fix or remove the file and restart."
                        ),
                        cx,
                    );
                });
                if shown.is_err() {
                    return;
                }
            }
        })
    }

    /// Reopens the saved tabs; a connection deleted since is left unset.
    fn restore(&mut self, saved: Session, window: &mut Window, cx: &mut Context<Self>) {
        let active = saved.active;
        self.sidebar = saved.sidebar;
        self.file_explorer
            .update(cx, |explorer, cx| explorer.restore(saved.folders, cx));
        for tab in saved.tabs {
            self.open_saved_tab(tab, window, cx);
        }
        self.activate(active.min(self.tabs.len().saturating_sub(1)), window, cx);
        self.last_saved = Some(self.session(cx));
    }

    /// Opens a tab as it was saved; a connection deleted since is left unset.
    fn open_saved_tab(&mut self, tab: SavedTab, window: &mut Window, cx: &mut Context<Self>) {
        // Two tabs on one file would overwrite each other's saves: the text
        // comes back, without its file.
        let file = tab.file.filter(|file| {
            !self
                .tabs
                .iter()
                .any(|open| open.view.read(cx).file() == Some(file.as_path()))
        });
        let connection = tab
            .connection
            .filter(|name| connections::find(cx, name).is_some())
            .map(SharedString::from);
        self.open_tab(
            ConnectionFields {
                connection,
                database: tab.database,
            },
            window,
            cx,
        );
        if let Some(view) = self.active_tab().cloned() {
            view.update(cx, |view, cx| {
                view.set_sql(&tab.sql, window, cx);
                if let Some(file) = file {
                    view.restore_file(file, tab.modified, cx);
                }
            });
        }
    }

    fn reopen_closed_tab(
        &mut self,
        _: &ReopenClosedTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.closed_tabs.pop() {
            Some(tab) => self.open_saved_tab(tab, window, cx),
            None => window.push_notification("No closed tab to reopen.", cx),
        }
    }

    fn open_file(&mut self, _: &OpenFile, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Open".into()),
        });
        self.open_file_task = Some(cx.spawn_in(window, async move |this, cx| {
            let paths = match chosen.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    tracing::warn!(%error, "open dialog failed");
                    return;
                }
            };
            let opened = this.update_in(cx, |_, window, cx| {
                Self::open_paths(paths, FileOpening::default(), window, cx);
            });
            if let Err(error) = opened {
                tracing::debug!(%error, "workspace closed before the files were chosen");
            }
        }));
    }

    fn open_folder(&mut self, _: &OpenFolder, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("Open Folder".into()),
        });
        self.open_folder_task = Some(cx.spawn_in(window, async move |this, cx| {
            let paths = match chosen.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    tracing::warn!(%error, "open folder dialog failed");
                    return;
                }
            };
            let opened = this.update(cx, |this, cx| {
                for path in paths {
                    this.show_folder(path, cx);
                }
            });
            if let Err(error) = opened {
                tracing::debug!(%error, "workspace closed before the folder was chosen");
            }
        }));
    }

    fn show_folder(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.file_explorer
            .update(cx, |explorer, cx| explorer.open_folder(path, cx));
        self.sidebar = SidebarView::Files;
        self.schedule_session_save(cx);
        cx.notify();
    }

    fn find_in_folders(&mut self, _: &FindInFolders, window: &mut Window, cx: &mut Context<Self>) {
        let roots = self.file_explorer.read(cx).folder_paths();
        self.open_find_dialog(roots, window, cx);
    }

    fn open_find_dialog(
        &mut self,
        roots: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if roots.is_empty() {
            window.push_notification("Open a folder to search its files.", cx);
            return;
        }
        let find = cx.new(|cx| FindInFolder::new(roots, window, cx));
        self.find_subscription = Some(cx.subscribe_in(
            &find,
            window,
            |_, _, event: &FindEvent, window, cx| match event {
                FindEvent::Open { path, line } => {
                    window.close_dialog(cx);
                    let opening = FileOpening {
                        run: false,
                        line: Some(*line),
                    };
                    Self::open_paths(vec![path.clone()], opening, window, cx);
                }
            },
        ));
        let focus = find.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .title("Find in folder")
                .w(px(760.))
                .child(find.clone())
        });
        window.defer(cx, move |window, cx| {
            focus.update(cx, |find, cx| find.focus_input(window, cx));
        });
    }

    fn on_file_explorer_event(
        &mut self,
        _: &Entity<FileExplorer>,
        event: &FileExplorerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            FileExplorerEvent::Open { path, run } => {
                let opening = FileOpening {
                    run: *run,
                    line: None,
                };
                Self::open_paths(vec![path.clone()], opening, window, cx);
            }
            FileExplorerEvent::Renamed { from, to } => {
                for tab in &self.tabs {
                    let moved = tab
                        .view
                        .read(cx)
                        .file()
                        .and_then(|file| folders::renamed_path(file, from, to));
                    if let Some(path) = moved {
                        tab.view.update(cx, |tab, cx| tab.move_file(path, cx));
                    }
                }
            }
            FileExplorerEvent::Deleted(path) => {
                for tab in &self.tabs {
                    let deleted = tab
                        .view
                        .read(cx)
                        .file()
                        .is_some_and(|file| file.starts_with(path));
                    if deleted {
                        tab.view.update(cx, QueryTab::forget_saved_state);
                    }
                }
            }
            FileExplorerEvent::FindInFolders(roots) => {
                self.open_find_dialog(roots.clone(), window, cx);
            }
            FileExplorerEvent::OpenFolder => self.open_folder(&OpenFolder, window, cx),
            FileExplorerEvent::FoldersChanged => self.schedule_session_save(cx),
        }
    }

    /// When a run starts, notes the tab's connection on its file's folder for the next file opened there.
    fn remember_folder_connection(&mut self, view: &Entity<QueryTab>, cx: &mut Context<Self>) {
        let running = view.read(cx).is_running();
        let Some(open) = self.tabs.iter_mut().find(|tab| tab.view == *view) else {
            return;
        };
        let started = running && !open.was_running;
        open.was_running = running;
        if !started {
            return;
        }
        let tab = view.read(cx);
        let Some(file) = tab.file().map(Path::to_path_buf) else {
            return;
        };
        let fields = tab.connection_fields(cx);
        self.file_explorer.update(cx, |explorer, _| {
            explorer.remember_connection(&file, &fields);
        });
    }

    /// Reads each file off the UI thread and shows it in a tab; a folder opens in the file tree.
    fn open_paths(
        paths: Vec<PathBuf>,
        opening: FileOpening,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Detached: a second drop must not stop the files of the first.
        cx.spawn_in(window, async move |this, cx| {
            for path in paths {
                let read = cx
                    .background_executor()
                    .spawn({
                        let path = path.clone();
                        async move { (!path.is_dir()).then(|| read_sql_file(&path)) }
                    })
                    .await;
                let shown = this.update_in(cx, |this, window, cx| match read {
                    None => this.show_folder(path, cx),
                    Some(Ok(file)) => this.show_file(path, &file, opening, window, cx),
                    Some(Err(message)) => window.push_notification(message, cx),
                });
                if shown.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// Shows `path` in its tab, or a blank or new one on its folder's last connection.
    fn show_file(
        &mut self,
        path: PathBuf,
        file: &FileText,
        opening: FileOpening,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let already_open = self
            .tabs
            .iter()
            .position(|tab| tab.view.read(cx).file() == Some(path.as_path()));
        if let Some(index) = already_open {
            self.activate(index, window, cx);
        } else {
            let folder_fields = self.file_explorer.read(cx).fields_for(&path);
            let blank = self
                .active_tab()
                .is_some_and(|tab| tab.read(cx).is_blank(cx));
            if blank {
                if let (Some(fields), Some(tab)) = (&folder_fields, self.active_tab().cloned()) {
                    tab.update(cx, |tab, cx| tab.use_fields(fields, window, cx));
                }
            } else {
                let fields = folder_fields
                    .or_else(|| {
                        self.active_tab()
                            .map(|tab| tab.read(cx).connection_fields(cx))
                    })
                    .unwrap_or_default();
                self.open_tab(fields, window, cx);
            }
            if let Some(tab) = self.active_tab().cloned() {
                tab.update(cx, |tab, cx| tab.open_file(path, file, window, cx));
            }
        }
        let Some(tab) = self.active_tab().cloned() else {
            return;
        };
        tab.update(cx, |tab, cx| {
            if let Some(line) = opening.line {
                tab.go_to_line(line, window, cx);
            }
            if opening.run {
                tab.run_everything(window, cx);
            }
        });
    }

    /// Cancels every running query on its server and waits briefly for the
    /// cancels to land: ending the process alone leaves PostgreSQL and
    /// Snowflake statements running.
    fn stop_queries_for_quit(&self, cx: &App) {
        let mut finished: Vec<_> = self
            .tabs
            .iter()
            .filter_map(|tab| tab.view.read(cx).cancel_for_quit())
            .collect();
        if finished.is_empty() {
            return;
        }
        tokio_bridge::block_on_for(cx, QUIT_CANCEL_WAIT, async move {
            for run in &mut finished {
                // An error means the sender is gone: the run has ended.
                while run.changed().await.is_ok() {}
            }
        });
    }

    fn session(&self, cx: &App) -> Session {
        Session {
            tabs: self
                .tabs
                .iter()
                .map(|tab| tab.view.read(cx).saved_tab(cx))
                .collect(),
            active: self.active_index,
            folders: self.file_explorer.read(cx).saved_folders(),
            sidebar: self.sidebar,
            last_version: Some(crate::release_notes::CURRENT_VERSION.to_owned()),
        }
    }

    /// Saves the session a moment after the last change, skipping unchanged ones.
    fn schedule_session_save(&mut self, cx: &mut Context<Self>) {
        if self.session_save.is_some() {
            return;
        }
        self.session_save = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SESSION_SAVE_DELAY).await;
            let saved = this.update(cx, |this, cx| {
                this.session_save = None;
                let current = this.session(cx);
                if this.last_saved.as_ref() != Some(&current) {
                    session::save(&current, cx);
                    this.last_saved = Some(current);
                }
            });
            if let Err(error) = saved {
                tracing::debug!(%error, "workspace closed before saving the session");
            }
        }));
    }

    fn show_history(&mut self, _: &ShowHistory, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self
            .active_tab()
            .and_then(|tab| tab.read(cx).connection_fields(cx).connection)
        else {
            window.push_notification("Pick a connection to see its history.", cx);
            return;
        };
        let entries = history::for_connection(cx, &connection);
        if entries.is_empty() {
            window.push_notification(format!("No queries run on {connection} yet."), cx);
            return;
        }
        let now = history::now();
        let items: Vec<CommandItem> = entries
            .iter()
            .map(|entry| history_item(entry, now))
            .collect();
        let entries: std::rc::Rc<[HistoryEntry]> = entries.into();
        let state = cx.new(|cx| CommandState::new(window, cx));
        let focus_state = state.clone();
        let workspace = cx.weak_entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let entries = entries.clone();
            let workspace = workspace.clone();
            let connection = connection.clone();
            let list = Command::new(&state)
                .placeholder("Search earlier queries")
                .max_h(px(460.))
                .items(items.iter().cloned())
                .on_confirm(move |index, window, cx| {
                    let Some(entry) = entries.get(index.row).cloned() else {
                        return;
                    };
                    window.close_dialog(cx);
                    let fields = ConnectionFields {
                        connection: Some(connection.clone()),
                        database: entry.database,
                    };
                    let opened = workspace.update(cx, |workspace, cx| {
                        workspace.open_tab(fields, window, cx);
                        if let Some(tab) = workspace.active_tab().cloned() {
                            tab.update(cx, |tab, cx| tab.set_sql(&entry.sql, window, cx));
                        }
                    });
                    if let Err(error) = opened {
                        tracing::debug!(%error, "workspace closed while history was open");
                    }
                });
            dialog.title("Query history").w(px(720.)).p_0().child(list)
        });
        window.defer(cx, move |window, cx| {
            focus_state.update(cx, |state, cx| state.focus(window, cx));
        });
    }

    fn on_explorer_event(
        &mut self,
        _: &Entity<ObjectExplorer>,
        event: &ExplorerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ExplorerEvent::UseConnection {
                connection,
                database,
            } => {
                self.use_connection_in_active_tab(connection, window, cx);
                if let (Some(database), Some(tab)) = (database, self.active_tab().cloned()) {
                    tab.update(cx, |tab, cx| tab.set_database(database, window, cx));
                }
            }
            ExplorerEvent::OpenQuery {
                connection,
                database,
                sql,
                run,
            } => {
                self.open_tab(
                    ConnectionFields {
                        connection: Some(connection.clone()),
                        database: database.to_string(),
                    },
                    window,
                    cx,
                );
                if let Some(tab) = self.active_tab().cloned() {
                    tab.update(cx, |tab, cx| {
                        tab.set_database(database, window, cx);
                        tab.set_sql(sql, window, cx);
                        if *run {
                            tab.run_everything(window, cx);
                        }
                    });
                }
            }
            ExplorerEvent::InsertText(text) => {
                if let Some(tab) = self.active_tab().cloned() {
                    tab.update(cx, |tab, cx| tab.insert_text(text, window, cx));
                }
            }
            ExplorerEvent::AddConnection => Self::open_connection_dialog(None, window, cx),
            ExplorerEvent::EditConnection(saved) => {
                Self::open_connection_dialog(Some(saved.clone()), window, cx);
            }
            ExplorerEvent::DeleteConnection(name) => {
                let name = name.clone();
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let name = name.clone();
                    alert
                        .confirm()
                        .title(format!("Delete connection {name}?"))
                        .description("Its saved password or secret is deleted too.")
                        .ok_text("Delete")
                        .ok_variant(ButtonVariant::Danger)
                        .on_ok(move |_, _, cx| {
                            connections::remove(cx, &name);
                            true
                        })
                });
            }
        }
    }

    fn use_connection_in_active_tab(
        &mut self,
        name: &SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.active_tab().cloned() {
            tab.update(cx, |tab, cx| tab.use_connection(name, window, cx));
            let editor_focus = tab.read(cx).editor_focus_handle(cx);
            editor_focus.focus(window, cx);
        }
    }

    /// Opens the add or edit dialog; `editing` names the connection being edited.
    fn open_connection_dialog(
        editing: Option<SavedConnection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let form = cx.new(|cx| ConnectionForm::new(editing.as_ref(), window, cx));
        let original_name = editing.map(|saved| saved.name);
        let title = if original_name.is_some() {
            "Edit connection"
        } else {
            "New connection"
        };
        let workspace = cx.weak_entity();
        let name_focus = form.focus_handle(cx);
        window.open_dialog(cx, move |dialog, _, _| {
            let form = form.clone();
            let original_name = original_name.clone();
            let workspace = workspace.clone();
            dialog
                .title(title)
                .w(px(640.))
                .child(form.clone())
                .footer(
                    DialogFooter::new()
                        .child(
                            DialogClose::new()
                                .child(Button::new("cancel").label("Cancel").outline()),
                        )
                        .child(
                            DialogAction::new().child(Button::new("save").label("Save").primary()),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    let saved = match form.read(cx).saved_connection(cx) {
                        Ok(saved) => saved,
                        Err(message) => {
                            window.push_notification(message, cx);
                            return false;
                        }
                    };
                    if connections::is_unreadable_name(cx, &saved.name) {
                        window.push_notification(
                            "connections.toml has an entry with this name that this version cannot read; choose another name.",
                            cx,
                        );
                        return false;
                    }
                    let name = saved.name.clone();
                    connections::save(cx, saved, original_name.as_deref());
                    let name = SharedString::from(name);
                    let updated = workspace.update(cx, |workspace, cx| {
                        workspace.use_connection_in_active_tab(&name, window, cx);
                    });
                    if let Err(error) = updated {
                        tracing::debug!(%error, "workspace closed while the dialog was open");
                    }
                    true
                })
        });
        window.defer(cx, move |window, cx| name_focus.focus(window, cx));
    }

    fn active_tab(&self) -> Option<&Entity<QueryTab>> {
        self.tabs.get(self.active_index).map(|tab| &tab.view)
    }

    fn open_tab(&mut self, fields: ConnectionFields, window: &mut Window, cx: &mut Context<Self>) {
        let number = self.next_tab_number;
        self.next_tab_number += 1;
        let view = cx.new(|cx| QueryTab::new(number, fields, window, cx));
        // Tab titles, the status line and the saved session come from the tab's state.
        let observation = cx.observe(&view, |this, view, cx| {
            this.remember_folder_connection(&view, cx);
            cx.notify();
            this.schedule_session_save(cx);
        });
        self.tabs.push(OpenTab {
            view,
            was_running: false,
            _observation: observation,
        });
        self.activate(self.tabs.len() - 1, window, cx);
    }

    fn activate(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let editor_focus = tab.view.read(cx).editor_focus_handle(cx);
        tab.view
            .update(cx, |tab, cx| tab.check_file_on_disk(window, cx));
        self.active_index = index;
        // Opening, closing and switching tabs all pass through here.
        self.schedule_session_save(cx);
        // Focus after this update so the new tab's editor exists in the element tree.
        window.defer(cx, move |window, cx| editor_focus.focus(window, cx));
        cx.notify();
    }

    fn new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        let fields = self
            .active_tab()
            .map(|tab| tab.read(cx).connection_fields(cx))
            .unwrap_or_default();
        self.open_tab(fields, window, cx);
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        self.close_tab_at(self.active_index, window, cx);
    }

    /// Closes the tab, asking first when that would lose unsaved changes or an open transaction.
    fn close_tab_at(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let view = tab.view.read(cx);
        let Some(question) = close_question(
            &view.title(cx),
            view.is_modified(),
            view.has_open_transaction(),
        ) else {
            self.close_tab_now(index, window, cx);
            return;
        };
        let closing = tab.view.downgrade();
        let workspace = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let closing = closing.clone();
            let workspace = workspace.clone();
            alert
                .confirm()
                .title(question.title.clone())
                .description(question.description)
                .ok_text("Close")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let closed = workspace.update(cx, |workspace, cx| {
                        // Other tabs may have opened or closed while the question showed.
                        let index = workspace
                            .tabs
                            .iter()
                            .position(|tab| tab.view.downgrade() == closing);
                        if let Some(index) = index {
                            workspace.close_tab_now(index, window, cx);
                        }
                    });
                    if let Err(error) = closed {
                        tracing::debug!(%error, "workspace closed while the question showed");
                    }
                    true
                })
        });
    }

    /// Whether the window may close now. With a transaction open in some tab
    /// it asks first, and closes the window itself on a yes.
    fn confirm_window_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let open_transactions = self
            .tabs
            .iter()
            .filter(|tab| tab.view.read(cx).has_open_transaction())
            .count();
        if open_transactions == 0 || self.close_confirmed {
            self.prepare_to_close(cx);
            return true;
        }
        let workspace = cx.weak_entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let workspace = workspace.clone();
            alert
                .confirm()
                .title(if open_transactions == 1 {
                    "Quit with a transaction open?".to_owned()
                } else {
                    format!("Quit with {open_transactions} transactions open?")
                })
                .description("The server rolls back what they changed.")
                .ok_text("Quit")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let prepared = workspace.update(cx, |workspace, cx| {
                        workspace.close_confirmed = true;
                        workspace.prepare_to_close(cx);
                    });
                    if let Err(error) = prepared {
                        tracing::debug!(%error, "workspace closed while the question showed");
                    }
                    window.remove_window();
                    true
                })
        });
        false
    }

    fn prepare_to_close(&self, cx: &App) {
        session::save_now(&self.session(cx), cx);
        self.stop_queries_for_quit(cx);
    }

    fn close_tab_now(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        let closed = self.tabs.remove(index);
        closed.view.update(cx, |tab, cx| {
            tab.stop_running_query(cx);
            tab.release_language_server(cx);
        });
        if !closed.view.read(cx).is_blank(cx) {
            self.closed_tabs.push(closed.view.read(cx).saved_tab(cx));
            let excess = self.closed_tabs.len().saturating_sub(MAX_CLOSED_TABS);
            self.closed_tabs.drain(..excess);
        }
        if self.tabs.is_empty() {
            let fields = closed.view.read(cx).connection_fields(cx);
            self.open_tab(fields, window, cx);
            return;
        }
        let active = if index < self.active_index || self.active_index >= self.tabs.len() {
            self.active_index.saturating_sub(1)
        } else {
            self.active_index
        };
        self.activate(active, window, cx);
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        let next = (self.active_index + 1) % self.tabs.len().max(1);
        self.activate(next, window, cx);
    }

    fn previous_tab(&mut self, _: &PreviousTab, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.tabs.len().max(1);
        let previous = (self.active_index + count - 1) % count;
        self.activate(previous, window, cx);
    }

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = self.tabs.iter().enumerate().map(|(index, tab)| {
            let query_tab = tab.view.read(cx);
            let title = if query_tab.is_running() {
                SharedString::from(format!("● {}", query_tab.title(cx)))
            } else {
                query_tab.title(cx)
            };
            Tab::new().label(title).suffix(
                Button::new(SharedString::from(format!("close-tab-{index}")))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip("Close tab (Ctrl+Shift+W)")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.close_tab_at(index, window, cx);
                    })),
            )
        });
        TabBar::new("query-tabs")
            .w_full()
            .selected_index(self.active_index)
            .on_click(cx.listener(|this, index: &usize, window, cx| {
                this.activate(*index, window, cx);
            }))
            .children(tabs)
            // gpui-kit draws `last_empty_space` right after the last tab, but only
            // when the bar has a suffix, so the suffix is an empty spacer.
            .last_empty_space(
                div().px_1().child(
                    Button::new("new-tab")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .tooltip("New tab (Ctrl+N)")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.new_tab(&NewTab, window, cx);
                        })),
                ),
            )
            .suffix(div())
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let views = [SidebarView::Files, SidebarView::Database];
        let selected = views
            .iter()
            .position(|view| *view == self.sidebar)
            .unwrap_or_default();
        let content = match self.sidebar {
            SidebarView::Files => self.file_explorer.clone().into_any_element(),
            SidebarView::Database => self.explorer.clone().into_any_element(),
        };
        v_flex()
            .size_full()
            .child(
                TabBar::new("sidebar-views")
                    .w_full()
                    .xsmall()
                    .selected_index(selected)
                    .on_click(cx.listener(move |this, index: &usize, _, cx| {
                        if let Some(view) = views.get(*index) {
                            this.sidebar = *view;
                            this.schedule_session_save(cx);
                            cx.notify();
                        }
                    }))
                    .child(Tab::new().label("Files"))
                    .child(Tab::new().label("Database")),
            )
            .child(div().flex_1().min_h_0().child(content))
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (status, vim_status, connection) = self
            .active_tab()
            .map(|tab| {
                let tab = tab.read(cx);
                (
                    tab.status_text(),
                    tab.vim_status(cx),
                    tab.connection_fields(cx).connection,
                )
            })
            .unwrap_or_default();
        let server_status = connection
            .and_then(|connection| lsp_servers::status(cx, &connection))
            .map(|(command, status)| match status {
                ServerStatus::Starting => format!("{command}: starting"),
                ServerStatus::Running => command,
                ServerStatus::Failed { message, retry_at } => {
                    let wait = retry_at.saturating_duration_since(std::time::Instant::now());
                    format!("{command}: {message}; retry in {}s", wait.as_secs())
                }
            });
        h_flex()
            .justify_between()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(cx.theme().status_bar_border)
            .bg(cx.theme().status_bar)
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(
                h_flex()
                    .gap_3()
                    .children(vim_status.map(|mode| {
                        div()
                            .min_w_16()
                            .text_color(cx.theme().foreground)
                            .child(mode)
                    }))
                    .child(status),
            )
            .children(server_status.map(|text| div().child(text)))
            .child(
                Button::new("open-settings")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Settings)
                    .tooltip("Settings (or ctrl-shift-p for every command)")
                    // Opened directly: with nothing focused an action would not reach the workspace.
                    .on_click(|_, window, cx| command_palette::open_settings(window, cx)),
            )
    }
}

impl Render for Workspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .key_context(WORKSPACE_CONTEXT)
            .on_action(cx.listener(Self::new_tab))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::previous_tab))
            .on_action(cx.listener(Self::show_history))
            .on_action(cx.listener(Self::open_file))
            .on_action(cx.listener(Self::reopen_closed_tab))
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(
                |_, _: &crate::release_notes::ShowReleaseNotes, window, cx| {
                    crate::release_notes::show(None, window, cx);
                },
            ))
            .on_action(cx.listener(Self::find_in_folders))
            .on_action(
                cx.listener(|_, _: &crate::updater::CheckForUpdates, window, cx| {
                    crate::updater::check(true, window, cx);
                }),
            )
            .on_drop(cx.listener(|_, dropped: &ExternalPaths, window, cx| {
                Self::open_paths(dropped.paths().to_vec(), FileOpening::default(), window, cx);
            }))
            .on_action(cx.listener(|_, _: &AddConnection, window, cx| {
                Self::open_connection_dialog(None, window, cx);
            }))
            .on_action(cx.listener(|_, _: &OpenCommandPalette, window, cx| {
                command_palette::open_command_palette(window, cx);
            }))
            .on_action(cx.listener(|_, _: &OpenSettings, window, cx| {
                command_palette::open_settings(window, cx);
            }))
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                h_flex().flex_1().min_h_0().child(
                    h_resizable("workspace-split")
                        .child(
                            resizable_panel()
                                .size(px(280.))
                                .size_range(px(180.)..px(600.))
                                .child(
                                    div()
                                        .size_full()
                                        .border_r_1()
                                        .border_color(cx.theme().border)
                                        .child(self.render_sidebar(cx)),
                                ),
                        )
                        .child(
                            resizable_panel().child(
                                v_flex()
                                    .size_full()
                                    .min_w_0()
                                    .child(self.render_tab_bar(cx))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_h_0()
                                            .children(self.active_tab().cloned()),
                                    ),
                            ),
                        ),
                ),
            )
            .child(self.render_status_bar(cx))
    }
}

struct CloseQuestion {
    title: String,
    description: &'static str,
}

/// What to ask before closing a tab, or `None` when closing it loses nothing.
fn close_question(title: &str, modified: bool, open_transaction: bool) -> Option<CloseQuestion> {
    let title = title.trim_end_matches('*');
    let (title, description) = match (modified, open_transaction) {
        (false, false) => return None,
        (true, false) => (
            format!("Close {title} without saving?"),
            "Its changes are not in the file yet.",
        ),
        (false, true) => (
            format!("Close {title} with a transaction open?"),
            "The server rolls back what the transaction changed.",
        ),
        (true, true) => (
            format!("Close {title} without saving, with a transaction open?"),
            "Its changes are not in the file yet, and the server rolls back what the transaction changed.",
        ),
    };
    Some(CloseQuestion { title, description })
}

/// Closed tabs kept for reopening.
const MAX_CLOSED_TABS: usize = 20;
/// The longest a quit waits for running queries to stop on their servers.
const QUIT_CANCEL_WAIT: Duration = Duration::from_secs(3);

/// Wait after a change before the session is written.
const SESSION_SAVE_DELAY: Duration = Duration::from_secs(2);

/// The statement's first line, with its database, age and outcome under it.
fn history_item(entry: &HistoryEntry, now: u64) -> CommandItem {
    let first_line = entry
        .sql
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let title: String = first_line.chars().take(120).collect();
    let outcome = match (entry.succeeded, entry.rows) {
        (false, _) => "failed".to_owned(),
        (true, Some(rows)) => format!("{rows} rows"),
        (true, None) => "done".to_owned(),
    };
    let detail = format!(
        "{} · {} · {outcome} · {:.1}s",
        if entry.database.is_empty() {
            "default database"
        } else {
            &entry.database
        },
        history::age(entry.at, now),
        Duration::from_millis(entry.elapsed_ms).as_secs_f64(),
    );
    let label = SharedString::from(title);
    CommandItem::new()
        .label(label.clone())
        .keywords([SharedString::from(entry.sql.clone())])
        .child(move |_, cx| {
            v_flex()
                .gap_0p5()
                .child(div().text_sm().child(label.clone()))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(detail.clone()),
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_asks_only_when_something_is_lost() {
        assert!(close_question("Query 1", false, false).is_none());
        let unsaved = close_question("report.sql*", true, false).unwrap();
        assert_eq!(unsaved.title, "Close report.sql without saving?");
        let transaction = close_question("2 · sales", false, true).unwrap();
        assert_eq!(
            transaction.title,
            "Close 2 · sales with a transaction open?"
        );
        assert!(close_question("a.sql*", true, true).is_some());
    }
}
