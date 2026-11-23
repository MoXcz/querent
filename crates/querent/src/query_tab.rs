use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::IndexPath;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::combobox::{Combobox, ComboboxEvent, ComboboxState};
use gpui_kit::component::input::{
    Copy, Cut, Editor, EditorState, InputEvent, InputState, MoveDown, MoveUp, Paste,
    RangeDecoration, RangeDecorationCollection, RangeDecorationStyle, Redo, RopeExt as _,
    TextDecoration, TextDecorationCollection, Undo,
};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::table::TableState;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, resizable_panel,
    v_flex, v_resizable,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FocusHandle, Focusable, HighlightStyle, Hsla,
    InteractiveElement as _, IntoElement, Keystroke, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Task, Window, actions, div, px,
};
use sql_db::{CancelHandle, Connection, ConnectionConfig, QueryError, QueryOutput, ResultSet};
use sql_text::vim::{Buffer, Edit, Key, Mode, Vim};
use tokio::sync::Mutex;

use crate::KEY_CONTEXT;
use crate::connections::{self, ConnectionStore};
use crate::database_picker::{DatabaseChoices, DatabaseSelect};
use crate::history;
use crate::lsp_servers::LspServers;
use crate::results_table::ResultsTable;
use crate::schema_cache::{self, SchemaCache};
use crate::settings::{self, SettingsStore};
use crate::sql_language::{SqlCompletions, SqlHover, TabTarget};
use crate::tokio_bridge;

mod files;
pub use files::{FileText, read_sql_file};
mod language;
mod results;

actions!(
    querent,
    [
        /// Run the selected SQL, or the statement under the cursor.
        RunQuery,
        /// Run everything in the editor.
        RunAll,
        /// Ask the server to stop the running query.
        CancelQuery,
        /// Save the shown result as a CSV file.
        ExportCsv,
        /// Save the shown result as a JSON file.
        ExportJson,
        /// Turn vim keys in the editor on or off.
        ToggleVimMode,
        /// Format the editor's SQL with the connection's language server.
        FormatSql,
        /// Save the tab to its `.sql` file, asking for one the first time.
        SaveFile,
        /// Save the tab to a `.sql` file chosen now.
        SaveFileAs,
        /// Load the database's tables and columns again for completion and hover.
        RefreshSchema,
        /// Forget this connection's Entra sign-in, so the next run asks again.
        SignOut,
        /// Copy the selection, or the whole line with nothing selected.
        CopySelectionOrLine,
        /// Cut the selection, or the whole line with nothing selected.
        CutSelectionOrLine,
        /// Paste; a whole line copied with nothing selected goes above the cursor's line.
        PasteSelectionOrLine
    ]
);

/// The last line copied or cut whole, so pasting that text inserts it as a line.
#[derive(Default)]
struct LineClip(Option<String>);

impl gpui_kit::Global for LineClip {}

type SharedConnection = Arc<Mutex<Connection>>;

const RUN_HIGHLIGHT_DURATION: Duration = Duration::from_millis(600);
const RUNNING_TICK: Duration = Duration::from_millis(100);
/// Outer error: the task died. Middle: connecting failed. Inner: the query
/// failed, with what the server returned before the error in the output.
type QueryOutcome =
    anyhow::Result<Result<(SharedConnection, QueryOutput, Result<(), QueryError>), QueryError>>;

enum QueryState {
    Idle,
    Running {
        started: Instant,
        cancel: CancelHandle,
        /// What ran, for the history entry when it finishes.
        run: RunRecord,
        /// Cancel was pressed; the server has not confirmed the stop yet.
        cancelling: bool,
        /// Closes when the run's task ends, so quitting can wait for a cancel to land.
        finished: tokio::sync::watch::Receiver<()>,
    },
    /// Stopped on request.
    Cancelled {
        elapsed: Duration,
    },
    Done {
        output: QueryOutput,
        elapsed: Duration,
        /// The run failed after returning `output`; the error is its last message.
        failed: bool,
        /// The line of the tab that error blames, 1-based.
        error_line: Option<usize>,
    },
    Failed {
        message: SharedString,
        /// The line of the tab the server blames, 1-based.
        line: Option<usize>,
    },
}

#[derive(Clone)]
struct RunRecord {
    sql: String,
    /// Lines of the tab before the text that was sent, to place a server's error line.
    lines_before: usize,
    connection: SharedString,
    database: String,
}

/// Connection choice a new tab starts with.
#[derive(Debug, Clone, Default)]
pub struct ConnectionFields {
    /// Name of a saved connection.
    pub connection: Option<SharedString>,
    pub database: String,
}

type ConnectionSelect = SelectState<Vec<SharedString>>;

type ServerView = (bool, Vec<lsp_types::Diagnostic>);

pub struct QueryTab {
    number: usize,
    focus_handle: FocusHandle,
    connection_select: Entity<ConnectionSelect>,
    database_select: Entity<DatabaseSelect>,
    /// The list the picker was last filled from, to tell when the cache has a newer one.
    database_list: Option<Arc<Vec<SharedString>>>,
    editor: Entity<EditorState>,
    results: Entity<TableState<ResultsTable>>,
    connection: Option<SharedConnection>,
    state: QueryState,
    running_query: Option<Task<()>>,
    /// Redraws the elapsed time while a query runs; ends by itself when it stops.
    running_ticker: Option<Task<()>>,
    /// Bumped per run so a replaced query's late result is ignored.
    query_generation: u64,
    /// Which lines the last run sent, shown in the status line.
    ran_lines: Option<SharedString>,
    /// The result set in the grid; shared with the grid and CSV export.
    shown_result: Option<Arc<ResultSet>>,
    /// Which result set of the last run the grid shows.
    shown_index: usize,
    /// The grid shows the run's messages instead of a result set.
    show_messages: bool,
    /// Text a row must contain to stay in the grid.
    filter_input: Entity<InputState>,
    /// Pending filter pass; replacing it restarts the delay.
    filter_task: Option<Task<()>>,
    /// The next run ignores the row limit ("Run again without limit").
    without_row_limit: bool,
    /// The last run left a transaction open on this tab's connection.
    open_transaction: bool,
    last_run: Option<RunRecord>,
    /// Outcome of the last export, shown in the status line until the next run.
    export_notice: Option<SharedString>,
    export_task: Option<Task<()>>,
    vim: Vim,
    /// Fill under the character at the cursor in normal and visual mode.
    block_cursor: Option<RangeDecorationCollection>,
    /// Draws the character under the block cursor in the background color, as vim does.
    block_cursor_text: Option<TextDecorationCollection>,
    /// The range and the caret and background colors the block cursor was drawn with.
    block_cursor_range: Option<(Range<usize>, Hsla, Hsla)>,
    /// Brief fill over the SQL that was sent to the server.
    run_highlight: Option<RangeDecorationCollection>,
    /// This tab's document URI for language servers.
    document: Option<lsp_types::Uri>,
    /// The connection whose server has this tab's document open.
    server_connection: Option<SharedString>,
    format_task: Option<Task<()>>,
    file: files::TabFile,
    /// Pending syntax check; replacing it restarts the delay.
    syntax_check: Option<Task<()>>,
    /// The server state and diagnostics the last syntax check used.
    seen_server: ServerView,
    run_highlight_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl QueryTab {
    pub fn new(
        number: usize,
        fields: ConnectionFields,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let connection_select = cx.new(|cx| {
            let names = connections::names(cx);
            let selected = fields
                .connection
                .as_ref()
                .and_then(|name| names.iter().position(|candidate| candidate == name))
                .map(IndexPath::new);
            SelectState::new(names, selected, window, cx)
        });
        let database_select = new_database_select(fields.database, window, cx);
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql".to_owned())
                .line_number(true)
                .soft_wrap(false)
                .placeholder("select top 100 * from ...")
        });
        let document = language::tab_document(number);
        let target = TabTarget {
            connection: connection_select.downgrade(),
            database: database_select.downgrade(),
            document: document.clone(),
        };
        editor.update(cx, |editor, _| {
            let lsp = editor.lsp_mut();
            lsp.completion_provider = Some(std::rc::Rc::new(SqlCompletions {
                target: target.clone(),
            }));
            lsp.hover_provider = Some(std::rc::Rc::new(SqlHover { target }));
        });
        let results = cx.new(|cx| ResultsTable::new_grid(window, cx));
        let filter_input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter rows"));
        let vim_interceptor = Self::intercept_vim_keys(cx);

        let subscriptions = vec![
            vim_interceptor,
            cx.subscribe_in(&connection_select, window, Self::on_connection_selected),
            cx.subscribe_in(&database_select, window, Self::on_database_chosen),
            // Tabbing to the picker loads its list too, not only a click.
            cx.on_focus_in(&database_select.focus_handle(cx), window, |this, _, cx| {
                this.request_databases(cx);
            }),
            cx.observe_global_in::<SchemaCache>(window, Self::on_schema_cache_changed),
            cx.observe_global_in::<ConnectionStore>(window, Self::on_connections_changed),
            // The editor notifies on every cursor move and caret blink.
            cx.observe(&editor, |this, _, cx| this.update_block_cursor(cx)),
            cx.observe_global::<SettingsStore>(Self::update_block_cursor),
            // Server diagnostics arrive on their own schedule.
            cx.observe_global::<LspServers>(Self::on_servers_changed),
            cx.subscribe(&filter_input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.schedule_filter(cx);
                }
            }),
            cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.schedule_syntax_check(cx);
                    this.refresh_modified(cx);
                }
            }),
        ];

        Self {
            number,
            focus_handle: cx.focus_handle(),
            connection_select,
            database_select,
            database_list: None,
            editor,
            results,
            connection: None,
            state: QueryState::Idle,
            running_query: None,
            running_ticker: None,
            query_generation: 0,
            ran_lines: None,
            shown_result: None,
            shown_index: 0,
            show_messages: false,
            filter_input,
            filter_task: None,
            without_row_limit: false,
            open_transaction: false,
            last_run: None,
            export_notice: None,
            export_task: None,
            vim: Vim::default(),
            block_cursor: None,
            block_cursor_text: None,
            block_cursor_range: None,
            run_highlight: None,
            run_highlight_task: None,
            syntax_check: None,
            seen_server: ServerView::default(),
            document,
            server_connection: None,
            format_task: None,
            file: files::TabFile::default(),
            _subscriptions: subscriptions,
        }
    }

    fn intercept_vim_keys(cx: &mut Context<Self>) -> Subscription {
        // Interceptors run before key bindings and text input, so normal mode
        // can claim keys the editor would otherwise insert or bind.
        let tab = cx.weak_entity();
        cx.intercept_keystrokes({
            move |event, window, cx| {
                let Some(tab) = tab.upgrade() else {
                    return;
                };
                let handled = tab.update(cx, |tab, cx| {
                    tab.handle_menu_keystroke(&event.keystroke, window, cx)
                        || tab.handle_vim_keystroke(&event.keystroke, window, cx)
                });
                if handled {
                    cx.stop_propagation();
                }
            }
        })
    }

    pub fn connection_fields(&self, cx: &App) -> ConnectionFields {
        ConnectionFields {
            connection: self.connection_select.read(cx).selected_value().cloned(),
            database: self.database(cx),
        }
    }

    /// Points this tab at a saved connection and its default database.
    pub fn use_connection(
        &mut self,
        name: &SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connection_select.update(cx, |select, cx| {
            select.set_selected_value(name, window, cx);
        });
        self.fill_default_database(name, window, cx);
    }

    /// Points this tab at the connection and database in `fields`.
    pub fn use_fields(
        &mut self,
        fields: &ConnectionFields,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(connection) = &fields.connection else {
            return;
        };
        self.use_connection(connection, window, cx);
        if !fields.database.is_empty() {
            self.set_database(&fields.database, window, cx);
        }
    }

    pub fn set_database(&mut self, database: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.fill_database_choices(database, window, cx);
        let value = SharedString::from(database.to_owned());
        self.database_select.update(cx, |select, cx| {
            if value.is_empty() {
                select.clear_selection(cx);
            } else {
                select.set_selected_values(&[value], window, cx);
            }
        });
        cx.notify();
    }

    fn database(&self, cx: &App) -> String {
        self.database_select
            .read(cx)
            .selected_value()
            .map(|database| database.trim().to_owned())
            .unwrap_or_default()
    }

    /// Refills the picker from the cached list, keeping `current` in it.
    fn fill_database_choices(
        &mut self,
        current: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let list = self
            .connection_select
            .read(cx)
            .selected_value()
            .cloned()
            .and_then(|connection| schema_cache::databases(cx, &connection));
        let names = list.as_deref().cloned().unwrap_or_default();
        self.database_list = list;
        let choices = DatabaseChoices::new(names, current.to_owned());
        let current = SharedString::from(current.to_owned());
        self.database_select.update(cx, |select, cx| {
            select.set_items(choices, window, cx);
            // New items start unfiltered: filter again by what is typed, or, with
            // nothing typed, point the selection at the current name's new row.
            let query = select.query(cx);
            if !query.is_empty() {
                select.set_query(query, window, cx);
            } else if !current.is_empty() {
                select.set_selected_values(&[current], window, cx);
            }
        });
    }

    /// Opening the picker loads the connection's databases, the list the explorer shows.
    fn request_databases(&mut self, cx: &mut Context<Self>) {
        if let Some(connection) = self.connection_select.read(cx).selected_value().cloned() {
            schema_cache::load_databases(cx, &connection);
        }
    }

    fn on_schema_cache_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let list = self
            .connection_select
            .read(cx)
            .selected_value()
            .cloned()
            .and_then(|connection| schema_cache::databases(cx, &connection));
        let same = match (&list, &self.database_list) {
            (Some(new), Some(old)) => Arc::ptr_eq(new, old),
            (None, None) => true,
            (Some(_), None) | (None, Some(_)) => false,
        };
        if !same {
            let current = self.database(cx);
            self.fill_database_choices(&current, window, cx);
        }
    }

    fn on_database_chosen(
        &mut self,
        _: &Entity<DatabaseSelect>,
        event: &ComboboxEvent<DatabaseChoices>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A typed name joins the list, so it shows as a plain choice next time.
        if let ComboboxEvent::Confirm(_) = event {
            let current = self.database(cx);
            self.fill_database_choices(&current, window, cx);
            // The next run opens another connection; the mark was about the old one.
            self.open_transaction = false;
        }
        cx.notify();
    }

    pub fn set_sql(&mut self, sql: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.set_value(sql.to_owned(), window, cx);
        });
        self.schedule_syntax_check(cx);
    }

    /// Inserts at the editor's cursor, replacing any selection.
    pub fn insert_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| {
            editor.replace(text.to_owned(), window, cx);
            editor.focus(window, cx);
        });
    }

    pub fn run_everything(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run_all(&RunAll, window, cx);
    }

    fn on_connection_selected(
        &mut self,
        _: &Entity<ConnectionSelect>,
        event: &SelectEvent<Vec<SharedString>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let SelectEvent::Confirm(Some(name)) = event else {
            return;
        };
        self.fill_default_database(name, window, cx);
        self.open_transaction = false;
        // The new connection may be a dialect that is checked, or one that is not.
        self.schedule_syntax_check(cx);
    }

    fn fill_default_database(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        // A file has no databases; a name picked on a server connection would linger.
        if connections::find(cx, name).is_some_and(|saved| saved.settings.driver.is_file()) {
            self.set_database("", window, cx);
            return;
        }
        let database = connections::find(cx, name)
            .and_then(|connection| connection.config().ok())
            .map(|config| config.database().to_owned())
            .filter(|database| !database.is_empty());
        if let Some(database) = database {
            self.set_database(&database, window, cx);
        } else {
            let current = self.database(cx);
            self.fill_database_choices(&current, window, cx);
        }
        cx.notify();
    }

    fn on_connections_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let names = connections::names(cx);
        self.connection_select.update(cx, |select, cx| {
            select.set_items(names, window, cx);
        });
        cx.notify();
    }

    /// Mode and pending keys for the status bar; `None` when vim is off.
    pub fn vim_status(&self, cx: &App) -> Option<String> {
        if !settings::get(cx).vim_mode {
            return None;
        }
        let pending = self.vim.pending_text();
        Some(if pending.is_empty() {
            self.vim.mode_label().to_owned()
        } else {
            format!("{} {pending}", self.vim.mode_label())
        })
    }

    /// `ctrl-n` and `ctrl-p` move through an open completion menu, as in vim;
    /// with the menu closed they keep their usual bindings (`ctrl-n` opens a tab).
    fn handle_menu_keystroke(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let modifiers = &keystroke.modifiers;
        let only_control =
            modifiers.control && !modifiers.shift && !modifiers.alt && !modifiers.platform;
        let action: Box<dyn gpui_kit::Action> = match keystroke.key.as_str() {
            "n" if only_control => Box::new(MoveDown),
            "p" if only_control => Box::new(MoveUp),
            _ => return false,
        };
        let editor = self.editor.read(cx);
        if !editor.completion_menu_state().open || !editor.focus_handle(cx).is_focused(window) {
            return false;
        }
        window.dispatch_action(action, cx);
        true
    }

    /// Whether a copy, cut or paste works on whole lines, as in VS Code: only in
    /// the editor, outside vim mode, with nothing selected.
    fn acts_on_line(&self, window: &Window, cx: &App) -> bool {
        !settings::get(cx).vim_mode
            && self.editor.focus_handle(cx).is_focused(window)
            && self.editor.read(cx).selected_range().is_empty()
    }

    fn copy_selection_or_line(
        &mut self,
        _: &CopySelectionOrLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.acts_on_line(window, cx) {
            cx.set_global(LineClip(None));
            window.dispatch_action(Box::new(Copy), cx);
            return;
        }
        let editor = self.editor.read(cx);
        let text = editor.value();
        let clip = sql_text::line_clip(&text, sql_text::line_at(&text, editor.cursor()));
        write_line_clip(clip, cx);
    }

    fn cut_selection_or_line(
        &mut self,
        _: &CutSelectionOrLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.acts_on_line(window, cx) {
            cx.set_global(LineClip(None));
            window.dispatch_action(Box::new(Cut), cx);
            return;
        }
        let editor = self.editor.read(cx);
        let text = editor.value();
        let range = sql_text::line_to_cut(&text, editor.cursor());
        if range.is_empty() {
            return;
        }
        write_line_clip(sql_text::line_clip(&text, range.clone()), cx);
        self.editor.update(cx, |editor, cx| {
            editor.set_selected_range(range, cx);
            editor.replace("", window, cx);
        });
    }

    fn paste_selection_or_line(
        &mut self,
        _: &PasteSelectionOrLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let clipboard = cx.read_from_clipboard().and_then(|item| item.text());
        let line = clipboard.filter(|text| {
            self.acts_on_line(window, cx)
                && cx.try_global::<LineClip>().and_then(|clip| clip.0.as_ref()) == Some(text)
        });
        let Some(line) = line else {
            window.dispatch_action(Box::new(Paste), cx);
            return;
        };
        self.editor.update(cx, |editor, cx| {
            let cursor = editor.cursor();
            let start = sql_text::line_at(&editor.value(), cursor).start;
            editor.set_selected_range(start..start, cx);
            editor.insert(line.clone(), window, cx);
            let moved = cursor + line.len();
            editor.set_selected_range(moved..moved, cx);
        });
    }

    fn handle_vim_keystroke(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !settings::get(cx).vim_mode || !self.editor.focus_handle(cx).is_focused(window) {
            return false;
        }
        let Some(key) = vim_key(keystroke) else {
            return false;
        };
        // Insert mode leaves every key but Escape to the editor; copying the buffer for each would slow typing.
        if self.vim.mode() == Mode::Insert && !matches!(key, Key::Escape) {
            return false;
        }
        let (text, cursor) = {
            let editor = self.editor.read(cx);
            (editor.value(), editor.cursor())
        };
        let outcome = self.vim.handle_key(
            key,
            Buffer {
                text: &text,
                cursor,
            },
        );
        if outcome.handled {
            self.editor.update(cx, |editor, cx| {
                for edit in outcome.edits {
                    apply_vim_edit(editor, edit, window, cx);
                }
            });
            self.update_block_cursor(cx);
            cx.notify();
        }
        outcome.handled
    }

    fn update_block_cursor(&mut self, cx: &mut Context<Self>) {
        let wanted = if settings::get(cx).vim_mode && self.vim.mode() != Mode::Insert {
            // Runs on every caret blink, so it reads one character, not the buffer.
            let editor = self.editor.read(cx);
            let cursor = editor.cursor();
            // A newline fills a one-space cell, so empty lines get a block too.
            // The end of the text has no cell, and keeps the thin caret.
            editor
                .text()
                .char_at(cursor)
                .map(|character| cursor..cursor + character.len_utf8())
        } else {
            None
        };
        // The colors are part of the key, so a theme switch redraws the block.
        let drawn = wanted
            .clone()
            .map(|range| (range, cx.theme().caret, cx.theme().background));
        if drawn == self.block_cursor_range {
            return;
        }
        self.block_cursor_range = drawn;
        let fills: Vec<RangeDecoration> = wanted
            .iter()
            .map(|range| {
                RangeDecoration::new(range.clone())
                    .with_style(RangeDecorationStyle::Fill)
                    .with_color(cx.theme().caret)
            })
            .collect();
        let inverted_text: Vec<TextDecoration> = wanted
            .into_iter()
            .map(|range| {
                TextDecoration::new(
                    range,
                    HighlightStyle {
                        color: Some(cx.theme().background),
                        ..HighlightStyle::default()
                    },
                )
            })
            .collect();
        if let Some(collection) = &self.block_cursor {
            collection.set(fills, cx);
        } else {
            let collection = self.editor.update(cx, |editor, cx| {
                editor.create_range_decorations_collection(fills, cx)
            });
            self.block_cursor = Some(collection);
        }
        if let Some(collection) = &self.block_cursor_text {
            collection.set(inverted_text, cx);
        } else {
            let collection = self.editor.update(cx, |editor, cx| {
                editor.create_decorations_collection(inverted_text, cx)
            });
            self.block_cursor_text = Some(collection);
        }
    }

    fn flash_run_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let decoration = RangeDecoration::new(range)
            .with_style(RangeDecorationStyle::Fill)
            .with_color(cx.theme().selection);
        if let Some(collection) = &self.run_highlight {
            collection.set(vec![decoration], cx);
        } else {
            let collection = self.editor.update(cx, |editor, cx| {
                editor.create_range_decorations_collection(vec![decoration], cx)
            });
            self.run_highlight = Some(collection);
        }
        // Replacing the task cancels an older flash's timer.
        self.run_highlight_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RUN_HIGHLIGHT_DURATION).await;
            let cleared = this.update(cx, |this, cx| {
                if let Some(collection) = &this.run_highlight {
                    collection.clear(cx);
                }
            });
            if let Err(error) = cleared {
                tracing::debug!(%error, "tab closed before the run highlight cleared");
            }
        }));
    }

    pub fn editor_focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, QueryState::Running { .. })
    }

    pub fn title(&self, cx: &App) -> SharedString {
        if let Some(title) = self.file_title() {
            return title;
        }
        let database = self.database(cx);
        if database.is_empty() {
            format!("Query {}", self.number).into()
        } else {
            format!("{} · {database}", self.number).into()
        }
    }

    /// Cancels the running query on the server and lets it finish in the
    /// background. Dropping the task instead would abort it mid-response and
    /// leave nothing to tell the server to stop.
    pub fn stop_running_query(&mut self, cx: &mut Context<Self>) {
        if let QueryState::Running { cancel, .. } = &self.state {
            cancel.cancel();
        }
        if let Some(task) = self.running_query.take() {
            task.detach();
        }
        if self.is_running() {
            self.state = QueryState::Idle;
            cx.notify();
        }
    }

    /// Asks the server to stop the running query as the app quits. The
    /// receiver closes once the run has ended and the cancel has been sent.
    pub fn cancel_for_quit(&self) -> Option<tokio::sync::watch::Receiver<()>> {
        let QueryState::Running {
            cancel, finished, ..
        } = &self.state
        else {
            return None;
        };
        cancel.cancel();
        Some(finished.clone())
    }

    fn connection_config(&self, cx: &Context<Self>) -> Result<ConnectionConfig, SharedString> {
        let ConnectionFields {
            connection,
            database,
        } = self.connection_fields(cx);
        let Some(name) = connection else {
            return Err("Pick a connection, or add one with + in the Connections sidebar.".into());
        };
        let saved = connections::find(cx, &name)
            .ok_or_else(|| SharedString::from(format!("Connection {name} no longer exists.")))?;
        let mut config = saved
            .config()
            .map_err(|error| SharedString::from(format!("Connection {name}: {error}")))?;
        if !database.is_empty() {
            config.set_database(database);
        }
        if config.requires_database() && config.database().is_empty() {
            return Err("Enter the database name.".into());
        }
        config.set_max_rows(self.row_limit(cx));
        Ok(config)
    }

    /// The selection if there is one, else the statement under the cursor.
    fn sql_under_cursor(&self, cx: &Context<Self>) -> Option<(String, Range<usize>)> {
        let editor = self.editor.read(cx);
        let text = editor.value();
        let selection = editor.selected_range();
        let range = if selection.is_empty() {
            sql_text::statement_at(&text, editor.cursor())?
        } else {
            selection
        };
        let sql = text.get(range.clone())?;
        (!sql.trim().is_empty()).then(|| (sql.to_owned(), range))
    }

    fn run_query(&mut self, _: &RunQuery, window: &mut Window, cx: &mut Context<Self>) {
        match self.sql_under_cursor(cx) {
            Some((sql, range)) => {
                let text = self.editor.read(cx).value();
                self.ran_lines = Some(line_span(&text, &range));
                let lines_before = text
                    .get(..range.start)
                    .map_or(0, |before| before.matches('\n').count());
                self.flash_run_range(range, cx);
                self.start_query(sql, lines_before, window, cx);
            }
            None => self.show_error("No statement under the cursor.".into(), cx),
        }
    }

    fn run_all(&mut self, _: &RunAll, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self.editor.read(cx).value().to_string();
        if sql.trim().is_empty() {
            return;
        }
        self.ran_lines = Some("all".into());
        self.flash_run_range(0..sql.len(), cx);
        self.start_query(sql, 0, window, cx);
    }

    fn show_error(&mut self, message: SharedString, cx: &mut Context<Self>) {
        if matches!(self.state, QueryState::Running { .. }) {
            return;
        }
        self.state = QueryState::Failed {
            message,
            line: None,
        };
        cx.notify();
    }

    fn row_limit(&self, cx: &App) -> usize {
        match (self.without_row_limit, settings::get(cx).row_limit) {
            (true, _) | (false, 0) => usize::MAX,
            (false, limit) => limit,
        }
    }

    fn start_query(
        &mut self,
        sql: String,
        lines_before: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Checked first: a refused run must not stop the one that is running.
        if self.is_read_only(cx)
            && let Some((offset, word)) = sql_text::read_only::first_write(&sql)
        {
            let line = lines_before + line_of(&sql, offset);
            let message = format!(
                "Line {line}: this connection is read-only and `{word}` can change data. Nothing was sent."
            );
            if self.is_running() {
                window.push_notification(message, cx);
                return;
            }
            self.state = QueryState::Failed {
                message: message.into(),
                line: Some(line),
            };
            cx.notify();
            return;
        }
        self.stop_running_query(cx);
        self.export_notice = None;
        let limit = self.row_limit(cx);
        let config = self.connection_config(cx);
        self.without_row_limit = false;
        let config = match config {
            Ok(config) => config,
            Err(message) => {
                self.show_error(message, cx);
                return;
            }
        };
        let fields = self.connection_fields(cx);
        let run = RunRecord {
            sql: sql.clone(),
            lines_before,
            connection: fields.connection.unwrap_or_default(),
            database: fields.database,
        };
        self.last_run = Some(run.clone());

        let reusable = self.connection.clone().filter(|connection| {
            connection
                .try_lock()
                .is_ok_and(|connection| connection.matches(&config))
        });
        let cancel = CancelHandle::new();
        let (running, finished) = tokio::sync::watch::channel(());
        let query = tokio_bridge::spawn(cx, {
            let cancel = cancel.clone();
            async move {
                let _running = running;
                let connection = if let Some(connection) = reusable {
                    connection
                } else {
                    // Connecting can wait minutes on a browser sign-in, so cancel stops it too.
                    tokio::select! {
                        connected = Connection::connect(config) => Arc::new(Mutex::new(connected?)),
                        () = cancel.cancelled() => return Err(QueryError::Cancelled),
                    }
                };
                let mut output = QueryOutput::default();
                let result = {
                    let mut connection = connection.lock().await;
                    connection.set_max_rows(limit);
                    connection.execute_into(&sql, &cancel, &mut output).await
                };
                Ok::<_, QueryError>((connection, output, result))
            }
        });

        self.state = QueryState::Running {
            started: Instant::now(),
            cancel,
            run,
            cancelling: false,
            finished,
        };
        self.query_generation += 1;
        let generation = self.query_generation;
        self.running_ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(RUNNING_TICK).await;
                let running = this.update(cx, |this, cx| {
                    cx.notify();
                    this.is_running()
                });
                if !matches!(running, Ok(true)) {
                    break;
                }
            }
        }));
        self.running_query = Some(cx.spawn_in(window, async move |this, cx| {
            let outcome = query.await;
            let updated = this.update(cx, |this, cx| {
                if this.query_generation == generation {
                    this.finish_query(outcome, cx);
                }
            });
            if let Err(error) = updated {
                tracing::debug!(%error, "query window closed before the query finished");
            }
        }));
        cx.notify();
    }

    fn finish_query(&mut self, outcome: QueryOutcome, cx: &mut Context<Self>) {
        let (elapsed, run, cancelling) = match &self.state {
            QueryState::Running {
                started,
                run,
                cancelling,
                ..
            } => (started.elapsed(), Some(run.clone()), *cancelling),
            QueryState::Idle
            | QueryState::Done { .. }
            | QueryState::Failed { .. }
            | QueryState::Cancelled { .. } => (Duration::ZERO, None, false),
        };
        self.running_query = None;
        self.open_transaction = left_transaction_open(&outcome);
        self.state = match outcome {
            Ok(Ok((connection, mut output, Ok(())))) => {
                self.connection = Some(connection);
                if cancelling {
                    output
                        .messages
                        .push("Finished before the cancel reached the server.".to_owned());
                }
                // The last result set is usually the one the batch was written for.
                let last = output.result_sets.len().saturating_sub(1);
                self.show_messages = false;
                self.show_result_set(&output, last, cx);
                QueryState::Done {
                    output,
                    elapsed,
                    failed: false,
                    error_line: None,
                }
            }
            Ok(Ok((connection, _, Err(QueryError::Cancelled)))) => {
                self.connection = Some(connection);
                QueryState::Cancelled { elapsed }
            }
            // Cancelled while connecting: nothing reusable was opened.
            Ok(Err(QueryError::Cancelled)) => {
                self.connection = None;
                QueryState::Cancelled { elapsed }
            }
            Ok(Ok((connection, mut output, Err(error)))) => {
                // A lost connection is not reused; any other error leaves it usable.
                self.connection =
                    (!matches!(error, QueryError::ConnectionLost(_))).then_some(connection);
                let line = error
                    .line()
                    .zip(run.as_ref())
                    .map(|(line, run)| run.lines_before + line);
                let message = match line {
                    Some(line) => format!("Line {line}: {error}"),
                    None => error.to_string(),
                };
                if output.result_sets.is_empty() && output.messages.is_empty() {
                    QueryState::Failed {
                        message: message.into(),
                        line,
                    }
                } else {
                    // Earlier statements answered: keep their results next to the error.
                    output.messages.push(message);
                    let last = output.result_sets.len().saturating_sub(1);
                    self.show_result_set(&output, last, cx);
                    self.show_messages = true;
                    QueryState::Done {
                        output,
                        elapsed,
                        failed: true,
                        error_line: line,
                    }
                }
            }
            Ok(Err(error)) => {
                self.connection = None;
                QueryState::Failed {
                    message: error.to_string().into(),
                    line: None,
                }
            }
            Err(error) => {
                self.connection = None;
                QueryState::Failed {
                    message: format!("query task failed: {error}").into(),
                    line: None,
                }
            }
        };
        if let Some(run) = run {
            self.record_run(run, elapsed, cx);
        }
        // The rows on screen are the previous run's, not this one's.
        if matches!(
            self.state,
            QueryState::Failed { .. } | QueryState::Cancelled { .. }
        ) {
            self.shown_result = None;
            self.results.update(cx, |table, cx| {
                table.delegate_mut().clear();
                table.refresh(cx);
                cx.notify();
            });
        }
        cx.notify();
    }

    /// Adds the run that just ended to the query history.
    fn record_run(&self, run: RunRecord, elapsed: Duration, cx: &mut Context<Self>) {
        let (succeeded, rows) = match &self.state {
            QueryState::Done { output, failed, .. } => (
                !failed,
                output.result_sets.last().map(|result| result.rows.len()),
            ),
            QueryState::Idle
            | QueryState::Running { .. }
            | QueryState::Failed { .. }
            | QueryState::Cancelled { .. } => (false, None),
        };
        history::record(
            cx,
            history::HistoryEntry {
                at: history::now(),
                connection: run.connection.to_string(),
                database: run.database,
                sql: run.sql,
                elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                rows,
                succeeded,
            },
        );
    }

    /// The tab as saved in the session file.
    pub fn saved_tab(&self, cx: &App) -> crate::session::SavedTab {
        let fields = self.connection_fields(cx);
        crate::session::SavedTab {
            connection: fields.connection.map(|name| name.to_string()),
            database: fields.database,
            sql: self.editor.read(cx).value().to_string(),
            file: self.file().map(std::path::Path::to_path_buf),
            modified: self.is_modified(),
        }
    }

    fn sign_out(&mut self, _: &SignOut, window: &mut Window, cx: &mut Context<Self>) {
        let request = self
            .connection_fields(cx)
            .connection
            .and_then(|name| connections::find(cx, &name))
            .and_then(|saved| saved.settings.authentication.sign_in_request());
        let Some(request) = request else {
            window.push_notification(
                "This connection does not use the Entra browser sign-in.",
                cx,
            );
            return;
        };
        // A running query keeps its connection and token; the next run opens a new one.
        if !self.is_running() {
            self.connection = None;
            self.open_transaction = false;
        }
        tokio_bridge::spawn(cx, async move { sql_db::entra::sign_out(&request).await }).detach();
        window.push_notification(
            "Signed out. The next run on this connection asks you to sign in again.",
            cx,
        );
    }

    fn cancel_query(&mut self, _: &CancelQuery, _: &mut Window, cx: &mut Context<Self>) {
        if let QueryState::Running {
            cancel, cancelling, ..
        } = &mut self.state
            && !*cancelling
        {
            cancel.cancel();
            *cancelling = true;
            cx.notify();
        }
    }

    /// SQLite opens one file and has no databases to pick.
    fn connection_is_file(&self, cx: &App) -> bool {
        self.connection_select
            .read(cx)
            .selected_value()
            .and_then(|name| connections::find(cx, name))
            .is_some_and(|saved| saved.settings.driver.is_file())
    }

    fn saved_connection<'a>(&self, cx: &'a App) -> Option<&'a connections::SavedConnection> {
        let name = self.connection_select.read(cx).selected_value()?;
        connections::find(cx, name)
    }

    /// The tab's connection is marked as production in its saved settings.
    fn is_production(&self, cx: &App) -> bool {
        self.saved_connection(cx)
            .is_some_and(|saved| saved.environment == connections::Environment::Production)
    }

    fn is_read_only(&self, cx: &App) -> bool {
        self.saved_connection(cx)
            .is_some_and(|saved| saved.access == connections::Access::ReadOnly)
    }

    /// The last run left a transaction open, which closing the tab rolls back.
    pub fn has_open_transaction(&self) -> bool {
        self.open_transaction
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = matches!(self.state, QueryState::Running { .. });
        h_flex()
            .gap_2()
            .p_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .w(px(240.))
                    .child(
                        Select::new(&self.connection_select)
                            .small()
                            .placeholder("Connection"),
                    ),
            )
            .children(self.is_production(cx).then(|| {
                div()
                    .px_2()
                    .rounded_sm()
                    .text_xs()
                    .text_color(cx.theme().danger_foreground)
                    .bg(cx.theme().danger)
                    .child("PRODUCTION")
            }))
            .children(self.is_read_only(cx).then(|| {
                div()
                    .px_2()
                    .rounded_sm()
                    .text_xs()
                    .border_1()
                    .border_color(cx.theme().border)
                    .text_color(cx.theme().muted_foreground)
                    .child("READ-ONLY")
            }))
            .children((!self.connection_is_file(cx)).then(|| {
                div()
                    .w(px(220.))
                    .capture_any_mouse_down(cx.listener(|this, _, _, cx| {
                        this.request_databases(cx);
                    }))
                    .child(
                        Combobox::new(&self.database_select)
                            .small()
                            .placeholder("database")
                            .search_placeholder("Search, or type a name"),
                    )
            }))
            .child(
                Button::new("run")
                    .primary()
                    .small()
                    .icon(LucideIcon::Play)
                    .loading(running)
                    .label(if running { "Running" } else { "Run" })
                    .tooltip("Run statement under cursor or selection (Ctrl+Enter). Run all: Ctrl+Shift+Enter")
                    .disabled(running)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.run_query(&RunQuery, window, cx)),
                    ),
            )
            .child(
                Button::new("cancel")
                    .small()
                    .icon(LucideIcon::CircleStop)
                    .label("Cancel")
                    .disabled(
                        !matches!(self.state, QueryState::Running { cancelling: false, .. }),
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.cancel_query(&CancelQuery, window, cx);
                    })),
            )
    }

    pub fn status_text(&self) -> String {
        let status = self.query_status_text();
        let status = if self.open_transaction {
            format!("Open transaction · {status}")
        } else {
            status
        };
        match &self.export_notice {
            Some(notice) => format!("{status} · {notice}"),
            None => status,
        }
    }

    fn query_status_text(&self) -> String {
        match &self.state {
            QueryState::Idle => "Ready".to_owned(),
            QueryState::Running {
                started,
                cancelling,
                ..
            } => format!(
                "{} {:.1}s",
                if *cancelling {
                    "Cancelling…"
                } else {
                    "Running…"
                },
                started.elapsed().as_secs_f64()
            ),
            QueryState::Cancelled { elapsed } => {
                format!("Cancelled · {:.2}s", elapsed.as_secs_f64())
            }
            QueryState::Failed { .. } => "Failed".to_owned(),
            QueryState::Done {
                output,
                elapsed,
                failed,
                error_line: _,
            } => {
                let mut parts = Vec::new();
                if *failed {
                    parts.push("Failed".to_owned());
                }
                if let Some(ran_lines) = &self.ran_lines {
                    parts.push(ran_lines.to_string());
                }
                if let Some(result_set) = output.result_sets.get(self.shown_index) {
                    parts.push(format!("{} rows", result_set.rows.len()));
                    if result_set.truncated {
                        parts.push("truncated".to_owned());
                    }
                }
                if output.result_sets.len() > 1 {
                    parts.push(format!("{} result sets", output.result_sets.len()));
                }
                if let Some(count) = output.rows_affected {
                    parts.push(format!("{count} rows affected"));
                }
                parts.push(format!("{:.2}s", elapsed.as_secs_f64()));
                // A failed run's last message is the error, too long for one line.
                if let Some(message) = output.messages.last().filter(|_| !failed) {
                    parts.push(message.clone());
                }
                parts.join(" · ")
            }
        }
    }
}

impl Focusable for QueryTab {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for QueryTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::run_query))
            .on_action(cx.listener(Self::run_all))
            .on_action(cx.listener(Self::cancel_query))
            .on_action(cx.listener(Self::export_csv))
            .on_action(cx.listener(Self::export_json))
            .on_action(cx.listener(Self::format_sql))
            .on_action(cx.listener(Self::save_file))
            .on_action(cx.listener(Self::save_file_as))
            .on_action(cx.listener(Self::refresh_schema))
            .on_action(cx.listener(Self::sign_out))
            .on_action(cx.listener(Self::copy_selection_or_line))
            .on_action(cx.listener(Self::cut_selection_or_line))
            .on_action(cx.listener(Self::paste_selection_or_line))
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_toolbar(cx))
            .child(
                // min_h_0 lets the split shrink so the workspace status bar stays on screen.
                div().flex_1().min_h_0().child(
                    v_resizable("editor-results")
                        .child(
                            resizable_panel()
                                .size(px(320.))
                                .child(Editor::new(&self.editor).bordered(false).h_full()),
                        )
                        .child(resizable_panel().child(self.render_results(cx))),
                ),
            )
    }
}

/// Only a run that kept its connection can have left a transaction on it.
fn left_transaction_open(outcome: &QueryOutcome) -> bool {
    match outcome {
        Ok(Ok((_, output, result))) => {
            output.open_transaction == Some(true)
                && !matches!(result, Err(QueryError::ConnectionLost(_)))
        }
        Ok(Err(_)) | Err(_) => false,
    }
}

fn new_database_select(
    database: String,
    window: &mut Window,
    cx: &mut Context<QueryTab>,
) -> Entity<DatabaseSelect> {
    cx.new(|cx| {
        let selected = (!database.is_empty())
            .then(|| IndexPath::new(0))
            .into_iter()
            .collect();
        ComboboxState::new(
            DatabaseChoices::new(Vec::new(), database),
            selected,
            window,
            cx,
        )
        .searchable(true)
    })
}

fn write_line_clip(clip: String, cx: &mut App) {
    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(clip.clone()));
    cx.set_global(LineClip(Some(clip)));
}

fn apply_vim_edit(
    editor: &mut EditorState,
    edit: Edit,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) {
    match edit {
        Edit::Replace { range, text } => {
            if range.is_empty() && text.is_empty() {
                return;
            }
            editor.set_selected_range(range, cx);
            editor.replace(text, window, cx);
        }
        Edit::Select { range, reversed } => {
            editor.set_selected_range_with_direction(range, reversed, cx);
        }
        Edit::Undo => window.dispatch_action(Box::new(Undo), cx),
        Edit::Redo => window.dispatch_action(Box::new(Redo), cx),
    }
}

/// `None` leaves the key to the editor and the app: arrows, function keys and
/// shortcuts with cmd, alt or ctrl-shift.
fn vim_key(keystroke: &Keystroke) -> Option<Key> {
    let modifiers = &keystroke.modifiers;
    if modifiers.platform || modifiers.alt || modifiers.function {
        return None;
    }
    let single_char = |text: &str| {
        let mut characters = text.chars();
        characters.next().filter(|_| characters.next().is_none())
    };
    if modifiers.control {
        if modifiers.shift {
            return None;
        }
        return match keystroke.key.as_str() {
            "[" => Some(Key::Escape),
            key => single_char(key).map(Key::Ctrl),
        };
    }
    match keystroke.key.as_str() {
        "escape" => Some(Key::Escape),
        "enter" => Some(Key::Enter),
        "backspace" => Some(Key::Backspace),
        "space" => Some(Key::Char(' ')),
        _ => keystroke
            .key_char
            .as_deref()
            .and_then(single_char)
            .map(Key::Char),
    }
}

/// The 1-based line of `text` that byte `offset` is on.
fn line_of(text: &str, offset: usize) -> usize {
    text.get(..offset)
        .map_or(0, |before| before.matches('\n').count())
        + 1
}

/// "line 3" or "lines 3–5", 1-based, for the byte range that ran.
fn line_span(text: &str, range: &Range<usize>) -> SharedString {
    let first = line_of(text, range.start);
    let last = line_of(text, range.end);
    if first == last {
        format!("line {first}").into()
    } else {
        format!("lines {first}–{last}").into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_line_span() {
        let text = "select 1;\nselect\n  2;";
        assert_eq!(line_span(text, &(0..8)), "line 1");
        assert_eq!(line_span(text, &(10..20)), "lines 2–3");
    }
}
