use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::base::input::{Diagnostic, DiagnosticSeverity};
use gpui_kit::component::IndexPath;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::combobox::{Combobox, ComboboxEvent, ComboboxState};
use gpui_kit::component::input::{
    Editor, EditorState, InputEvent, MoveDown, MoveUp, RangeDecoration, RangeDecorationCollection,
    RangeDecorationStyle, Redo, Rope, RopeExt as _, Undo,
};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, resizable_panel,
    v_flex, v_resizable,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, AsyncApp, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, Keystroke, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Task, WeakEntity, Window, actions, div, prelude::FluentBuilder as _,
    px,
};
use sql_db::{
    CancelHandle, Connection, ConnectionConfig, Dialect, QueryError, QueryOutput, ResultSet,
};
use sql_text::format::FormatDialect;
use sql_text::vim::{Buffer, Edit, Key, Mode, Vim};
use tokio::sync::Mutex;

use crate::KEY_CONTEXT;
use crate::connections::{self, ConnectionStore};
use crate::database_picker::{DatabaseChoices, DatabaseSelect};
use crate::history;
use crate::lsp_servers::{self, LspServers};
use crate::results_table::ResultsTable;
use crate::schema_cache::{self, SchemaCache};
use crate::settings::{self, SettingsStore};
use crate::sql_language::{SqlCompletions, SqlHover, TabTarget};
use crate::tokio_bridge;

actions!(
    sql_editor,
    [
        /// Run the selected SQL, or the statement under the cursor.
        RunQuery,
        /// Run everything in the editor.
        RunAll,
        /// Ask the server to stop the running query.
        CancelQuery,
        /// Save the shown result as a CSV file.
        ExportCsv,
        /// Turn vim keys in the editor on or off.
        ToggleVimMode,
        /// Format the editor's SQL with the connection's language server.
        FormatSql
    ]
);

type SharedConnection = Arc<Mutex<Connection>>;

const BLOCK_CURSOR_OPACITY: f32 = 0.45;
const RUN_HIGHLIGHT_DURATION: Duration = Duration::from_millis(600);
const RUNNING_TICK: Duration = Duration::from_millis(100);
/// Pause after typing before the syntax is checked again.
const SYNTAX_CHECK_DELAY: Duration = Duration::from_millis(400);
const SYNTAX_CHECK_MAX_BYTES: usize = 256 * 1024;
/// Outer error: the task died. Middle: connecting failed. Inner: the query failed.
type QueryOutcome =
    anyhow::Result<Result<(SharedConnection, Result<QueryOutput, QueryError>), QueryError>>;

enum QueryState {
    Idle,
    Running {
        started: Instant,
        cancel: CancelHandle,
        /// What ran, for the history entry when it finishes.
        run: RunRecord,
    },
    Done {
        output: QueryOutput,
        elapsed: Duration,
    },
    Failed {
        message: SharedString,
    },
}

#[derive(Clone)]
struct RunRecord {
    sql: String,
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
    /// The next run ignores the row limit ("Run again without limit").
    without_row_limit: bool,
    last_run: Option<RunRecord>,
    /// Outcome of the last export, shown in the status line until the next run.
    export_notice: Option<SharedString>,
    export_task: Option<Task<()>>,
    vim: Vim,
    /// Fill under the character at the cursor in normal and visual mode.
    block_cursor: Option<RangeDecorationCollection>,
    block_cursor_range: Option<Range<usize>>,
    /// Brief fill over the SQL that was sent to the server.
    run_highlight: Option<RangeDecorationCollection>,
    /// This tab's document URI for language servers.
    document: Option<lsp_types::Uri>,
    /// The connection whose server has this tab's document open.
    server_connection: Option<SharedString>,
    format_task: Option<Task<()>>,
    /// Pending syntax check; replacing it restarts the delay.
    syntax_check: Option<Task<()>>,
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
        let document = tab_document(number);
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
        let results = cx.new(|cx| TableState::new(ResultsTable::default(), window, cx));
        // Interceptors run before key bindings and text input, so normal mode
        // can claim keys the editor would otherwise insert or bind.
        let tab = cx.weak_entity();
        let vim_interceptor = cx.intercept_keystrokes({
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
        });

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
            cx.observe_global::<LspServers>(Self::check_syntax),
            cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.schedule_syntax_check(cx);
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
            without_row_limit: false,
            last_run: None,
            export_notice: None,
            export_task: None,
            vim: Vim::default(),
            block_cursor: None,
            block_cursor_range: None,
            run_highlight: None,
            run_highlight_task: None,
            syntax_check: None,
            document,
            server_connection: None,
            format_task: None,
            _subscriptions: subscriptions,
        }
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

    /// The SQL dialect of the tab's connection, if it has one.
    fn dialect(&self, cx: &App) -> Option<Dialect> {
        let connection = self.connection_fields(cx).connection?;
        connections::find(cx, &connection).map(connections::SavedConnection::dialect)
    }

    fn schedule_syntax_check(&mut self, cx: &mut Context<Self>) {
        self.syntax_check = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SYNTAX_CHECK_DELAY).await;
            if let Err(error) = this.update(cx, Self::check_syntax) {
                tracing::debug!(%error, "tab closed before the syntax check");
            }
        }));
    }

    /// Marks syntax errors in the editor. T-SQL is skipped: the grammar does
    /// not know `TOP`, `[names]`, `GO` or `DECLARE` and would flag valid code.
    fn check_syntax(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        // Parsing runs on this thread, fast for an editor-sized buffer; a
        // huge pasted script is not checked rather than stalling typing.
        let checked = !matches!(self.dialect(cx), None | Some(Dialect::TSql))
            && text.len() <= SYNTAX_CHECK_MAX_BYTES;
        let errors = if checked {
            sql_text::syntax::syntax_errors(&text)
        } else {
            Vec::new()
        };
        let connection = self.connection_fields(cx).connection;
        if self.server_connection != connection {
            self.release_language_server(cx);
            self.server_connection.clone_from(&connection);
        }
        let server_diagnostics = match (&connection, &self.document) {
            (Some(connection), Some(document)) => {
                lsp_servers::sync_document(cx, connection, document, &text);
                lsp_servers::diagnostics(cx, connection, document.as_str())
            }
            _ => Vec::new(),
        };
        let rope = Rope::from(text.as_str());
        self.editor.update(cx, |editor, cx| {
            let Some(diagnostics) = editor.diagnostics_mut() else {
                return;
            };
            diagnostics.reset(&rope);
            for diagnostic in server_diagnostics {
                diagnostics.push(Diagnostic::from(diagnostic));
            }
            for error in errors {
                let range = rope.offset_to_position(error.range.start)
                    ..rope.offset_to_position(error.range.end);
                diagnostics.push(
                    Diagnostic::new(range, error.message)
                        .with_severity(DiagnosticSeverity::Error)
                        .with_source("sql"),
                );
            }
            cx.notify();
        });
    }

    /// Closes this tab's document on the language server it was sent to.
    pub fn release_language_server(&mut self, cx: &mut Context<Self>) {
        if let (Some(connection), Some(document)) = (self.server_connection.take(), &self.document)
        {
            lsp_servers::close_document(cx, &connection, document);
        }
    }

    /// The built-in formatter, or the language server when settings ask for it.
    fn format_sql(&mut self, _: &FormatSql, window: &mut Window, cx: &mut Context<Self>) {
        if settings::get(cx).format_with_language_server {
            self.format_with_server(window, cx);
        } else {
            self.format_built_in(window, cx);
        }
    }

    /// Formats the selection, or the whole buffer, as one undoable edit.
    fn format_built_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dialect = match self.dialect(cx) {
            Some(Dialect::TSql) => FormatDialect::SqlServer,
            Some(Dialect::Postgres) => FormatDialect::Postgres,
            Some(Dialect::Sqlite | Dialect::Snowflake) | None => FormatDialect::Generic,
        };
        let style = settings::get(cx).formatter.clone();
        let (text, selection, cursor) = {
            let editor = self.editor.read(cx);
            (
                editor.value().to_string(),
                editor.selected_range(),
                editor.cursor(),
            )
        };
        let whole = selection.is_empty();
        let range = if whole { 0..text.len() } else { selection };
        let Some(source) = text.get(range.clone()) else {
            return;
        };
        let Some(formatted) = sql_text::format::format_sql(source, dialect, &style) else {
            window.push_notification(
                "Left unchanged: the formatter could not read this SQL without changing it.",
                cx,
            );
            return;
        };
        if formatted == source {
            return;
        }
        self.editor.update(cx, |editor, cx| {
            editor.set_selected_range(range, cx);
            editor.replace(formatted, window, cx);
            // A whole-tab format should not throw the cursor to the end.
            if whole {
                let length = editor.value().len();
                let mut offset = cursor.min(length);
                let value = editor.value();
                while offset > 0 && !value.is_char_boundary(offset) {
                    offset -= 1;
                }
                editor.set_selected_range(offset..offset, cx);
            }
        });
    }

    /// Replaces the editor's text with the server's formatting, as one undoable edit.
    fn format_with_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(connection) = self.connection_fields(cx).connection else {
            window.push_notification("Pick a connection first.", cx);
            return;
        };
        let client = lsp_servers::client(cx, &connection);
        let status = lsp_servers::status(cx, &connection);
        let (Some(client), Some(document)) = (client, self.document.clone()) else {
            let message = match status {
                Some((command, lsp_servers::ServerStatus::Starting)) => {
                    format!("{command} is starting; try again in a moment.")
                }
                Some((command, lsp_servers::ServerStatus::Failed { message, .. })) => {
                    format!("{command}: {message}")
                }
                Some((_, lsp_servers::ServerStatus::Running)) | None => "Format SQL needs a language server for this connection's database (see language_servers in settings.toml).".to_owned(),
            };
            window.push_notification(message, cx);
            return;
        };
        if !lsp_servers::can_format(client.capabilities()) {
            window.push_notification("This language server does not format SQL.", cx);
            return;
        }
        let text = self.editor.read(cx).value().to_string();
        lsp_servers::sync_document(cx, &connection, &document, &text);
        let params = lsp_types::DocumentFormattingParams {
            text_document: lsp_types::TextDocumentIdentifier::new(document),
            options: lsp_types::FormattingOptions {
                tab_size: 4,
                insert_spaces: true,
                ..lsp_types::FormattingOptions::default()
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
        };
        let request = tokio_bridge::spawn(cx, async move {
            client
                .request::<lsp_types::request::Formatting>(params)
                .await
        });
        self.format_task = Some(cx.spawn_in(window, async move |this, cx| {
            let edits = match request.await {
                Ok(Ok(edits)) => edits.unwrap_or_default(),
                Ok(Err(error)) => {
                    tracing::debug!(%error, "format request");
                    return;
                }
                Err(error) => {
                    tracing::debug!(%error, "format task");
                    return;
                }
            };
            let applied = this.update_in(cx, |this, window, cx| {
                let current = this.editor.read(cx).value().to_string();
                // The user kept typing; these edits are for older text.
                if current != text || edits.is_empty() {
                    return;
                }
                let formatted = apply_text_edits(&current, &edits);
                this.editor.update(cx, |editor, cx| {
                    editor.set_selected_range(0..current.len(), cx);
                    editor.replace(formatted, window, cx);
                });
            });
            if let Err(error) = applied {
                tracing::debug!(%error, "tab closed before formatting finished");
            }
        }));
    }

    fn update_block_cursor(&mut self, cx: &mut Context<Self>) {
        let wanted = if settings::get(cx).vim_mode && self.vim.mode() != Mode::Insert {
            let editor = self.editor.read(cx);
            let text = editor.value();
            let cursor = editor.cursor();
            let end = text
                .get(cursor..)
                .and_then(|rest| rest.chars().next())
                .filter(|character| *character != '\n')
                .map(|character| cursor + character.len_utf8());
            end.map(|end| cursor..end)
        } else {
            None
        };
        if wanted == self.block_cursor_range {
            return;
        }
        self.block_cursor_range.clone_from(&wanted);
        let color = cx.theme().caret.opacity(BLOCK_CURSOR_OPACITY);
        let decorations: Vec<RangeDecoration> = wanted
            .into_iter()
            .map(|range| {
                RangeDecoration::new(range)
                    .with_style(RangeDecorationStyle::Fill)
                    .with_color(color)
            })
            .collect();
        if let Some(collection) = &self.block_cursor {
            collection.set(decorations, cx);
        } else {
            let collection = self.editor.update(cx, |editor, cx| {
                editor.create_range_decorations_collection(decorations, cx)
            });
            self.block_cursor = Some(collection);
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
                self.flash_run_range(range, cx);
                self.start_query(sql, window, cx);
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
        self.start_query(sql, window, cx);
    }

    fn show_error(&mut self, message: SharedString, cx: &mut Context<Self>) {
        if matches!(self.state, QueryState::Running { .. }) {
            return;
        }
        self.state = QueryState::Failed { message };
        cx.notify();
    }

    fn row_limit(&self, cx: &App) -> usize {
        match (self.without_row_limit, settings::get(cx).row_limit) {
            (true, _) | (false, 0) => usize::MAX,
            (false, limit) => limit,
        }
    }

    fn start_query(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
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
        let query = tokio_bridge::spawn(cx, {
            let cancel = cancel.clone();
            async move {
                let connection = if let Some(connection) = reusable {
                    connection
                } else {
                    // Connecting can wait minutes on a browser sign-in, so cancel stops it too.
                    tokio::select! {
                        connected = Connection::connect(config) => Arc::new(Mutex::new(connected?)),
                        () = cancel.cancelled() => return Err(QueryError::Cancelled),
                    }
                };
                let output = {
                    let mut connection = connection.lock().await;
                    connection.set_max_rows(limit);
                    connection.execute(&sql, &cancel).await
                };
                Ok::<_, QueryError>((connection, output))
            }
        });

        self.state = QueryState::Running {
            started: Instant::now(),
            cancel,
            run,
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
        let (elapsed, run) = match &self.state {
            QueryState::Running { started, run, .. } => (started.elapsed(), Some(run.clone())),
            QueryState::Idle | QueryState::Done { .. } | QueryState::Failed { .. } => {
                (Duration::ZERO, None)
            }
        };
        self.running_query = None;
        self.state = match outcome {
            Ok(Ok((connection, Ok(output)))) => {
                self.connection = Some(connection);
                // The last result set is usually the one the batch was written for.
                let last = output.result_sets.len().saturating_sub(1);
                self.show_messages = false;
                self.show_result_set(&output, last, cx);
                QueryState::Done { output, elapsed }
            }
            Ok(Ok((connection, Err(error)))) => {
                // A lost connection is not reused; any other error leaves it usable.
                self.connection =
                    (!matches!(error, QueryError::ConnectionLost(_))).then_some(connection);
                QueryState::Failed {
                    message: error.to_string().into(),
                }
            }
            Ok(Err(error)) => {
                self.connection = None;
                QueryState::Failed {
                    message: error.to_string().into(),
                }
            }
            Err(error) => {
                self.connection = None;
                QueryState::Failed {
                    message: format!("query task failed: {error}").into(),
                }
            }
        };
        if let Some(run) = run {
            let (succeeded, rows) = match &self.state {
                QueryState::Done { output, .. } => (
                    true,
                    output.result_sets.last().map(|result| result.rows.len()),
                ),
                QueryState::Idle | QueryState::Running { .. } | QueryState::Failed { .. } => {
                    (false, None)
                }
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
        if matches!(self.state, QueryState::Failed { .. }) {
            self.shown_result = None;
            self.results.update(cx, |table, cx| {
                table.delegate_mut().clear();
                table.refresh(cx);
                cx.notify();
            });
        }
        cx.notify();
    }

    fn show_result_set(&mut self, output: &QueryOutput, index: usize, cx: &mut Context<Self>) {
        let shown = output.result_sets.get(index).cloned().unwrap_or_default();
        self.show_result(index, shown, cx);
    }

    fn show_result(&mut self, index: usize, shown: ResultSet, cx: &mut Context<Self>) {
        self.shown_index = index;
        let shown = Arc::new(shown);
        self.shown_result = Some(shown.clone());
        self.results.update(cx, |table, cx| {
            table.delegate_mut().set_result(shown);
            table.refresh(cx);
            cx.notify();
        });
        cx.notify();
    }

    fn select_result_set(&mut self, index: usize, cx: &mut Context<Self>) {
        self.show_messages = false;
        let QueryState::Done { output, .. } = &self.state else {
            return;
        };
        let Some(shown) = output.result_sets.get(index).cloned() else {
            return;
        };
        self.show_result(index, shown, cx);
    }

    /// Runs the last statement again without the row limit, after a truncated result.
    fn rerun_without_limit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(run) = self.last_run.clone() else {
            return;
        };
        let fields = self.connection_fields(cx);
        if fields.connection.as_ref() != Some(&run.connection) || fields.database != run.database {
            window.push_notification(
                "The tab's connection or database changed since that run; run it again yourself.",
                cx,
            );
            return;
        }
        self.without_row_limit = true;
        self.start_query(run.sql, window, cx);
    }

    /// The tab as saved in the session file.
    pub fn saved_tab(&self, cx: &App) -> crate::session::SavedTab {
        let fields = self.connection_fields(cx);
        crate::session::SavedTab {
            connection: fields.connection.map(|name| name.to_string()),
            database: fields.database,
            sql: self.editor.read(cx).value().to_string(),
        }
    }

    fn cancel_query(&mut self, _: &CancelQuery, _: &mut Window, cx: &mut Context<Self>) {
        if let QueryState::Running { cancel, .. } = &self.state {
            cancel.cancel();
            cx.notify();
        }
    }

    fn export_csv(&mut self, _: &ExportCsv, _: &mut Window, cx: &mut Context<Self>) {
        let Some(result_set) = self
            .shown_result
            .clone()
            .filter(|result| !result.columns.is_empty())
        else {
            self.export_notice = Some("Nothing to export: run a query that returns rows.".into());
            cx.notify();
            return;
        };
        let directory = export_directory();
        let suggested_name = export_file_name(self.number, &self.connection_fields(cx).database);
        let chosen_path = cx.prompt_for_new_path(&directory, Some(&suggested_name));

        self.export_task = Some(cx.spawn(async move |this, cx| {
            let path = match chosen_path.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => {
                    let notice = format!("Export failed: no save dialog ({error}).");
                    report_export(&this, notice, cx);
                    return;
                }
            };
            let written = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    let result_set = result_set.clone();
                    async move { write_csv_file(&path, &result_set) }
                })
                .await;
            let notice = match written {
                Ok(()) => {
                    let mut notice = format!(
                        "Exported {} rows to {}",
                        result_set.rows.len(),
                        path.display()
                    );
                    if result_set.truncated {
                        notice.push_str(" (only the rows shown; the result was truncated)");
                    }
                    notice
                }
                Err(error) => format!("Export failed: {error:#}"),
            };
            report_export(&this, notice, cx);
        }));
    }

    /// SQLite opens one file and has no databases to pick.
    fn connection_is_file(&self, cx: &App) -> bool {
        self.connection_select
            .read(cx)
            .selected_value()
            .and_then(|name| connections::find(cx, name))
            .is_some_and(|saved| saved.settings.driver.is_file())
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
                    .disabled(!running)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.cancel_query(&CancelQuery, window, cx);
                    })),
            )
    }

    /// A spinner, the elapsed time and Cancel above the grid while a query runs.
    fn render_running_banner(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let QueryState::Running { started, .. } = &self.state else {
            return None;
        };
        Some(
            h_flex()
                .gap_2()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().accent)
                .text_sm()
                .child(Spinner::new().small())
                .child(format!("Running… {:.1}s", started.elapsed().as_secs_f64()))
                .child(div().flex_1())
                .child(
                    Button::new("cancel-running")
                        .xsmall()
                        .icon(LucideIcon::CircleStop)
                        .label("Cancel")
                        .tooltip("Stop the query on the server (Ctrl+Shift+C)")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.cancel_query(&CancelQuery, window, cx);
                        })),
                ),
        )
    }

    /// Row count and the no-limit rerun under the grid, shown while it holds a result.
    fn render_results_footer(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let result = self
            .shown_result
            .as_ref()
            .filter(|result| !result.columns.is_empty())?;
        let mut summary = format!("{} rows", result.rows.len());
        let truncated = result.truncated;
        if truncated {
            summary.push_str(" (first rows only: row limit reached)");
        }
        Some(
            h_flex()
                .justify_between()
                .px_2()
                .py_1()
                .border_t_1()
                .border_color(cx.theme().border)
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(summary)
                .children(truncated.then(|| {
                    Button::new("rerun-without-limit")
                        .ghost()
                        .xsmall()
                        .label("Run again without limit")
                        .tooltip("Runs the same SQL again and keeps every row")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.rerun_without_limit(window, cx);
                        }))
                })),
        )
    }

    fn render_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.state {
            QueryState::Failed { message } => div()
                .size_full()
                .p_3()
                .text_color(cx.theme().danger)
                .child(message.clone())
                .into_any_element(),
            QueryState::Done { output, .. } if output.result_sets.is_empty() => div()
                .size_full()
                .p_3()
                .text_color(cx.theme().muted_foreground)
                .child(match output.rows_affected {
                    Some(count) => format!("{count} rows affected."),
                    None => "Statement completed.".to_owned(),
                })
                .into_any_element(),
            QueryState::Done { output, .. } if self.show_messages => v_flex()
                .size_full()
                .child(self.render_result_switcher(output, cx))
                .child(
                    v_flex()
                        .flex_1()
                        .min_h_0()
                        .p_3()
                        .gap_1()
                        .text_sm()
                        .children(
                            output
                                .messages
                                .iter()
                                .map(|message| div().child(message.clone())),
                        ),
                )
                .into_any_element(),
            QueryState::Idle | QueryState::Running { .. } | QueryState::Done { .. } => v_flex()
                .size_full()
                .children(self.render_running_banner(cx))
                .children(match &self.state {
                    QueryState::Done { output, .. } => {
                        Some(self.render_result_switcher(output, cx))
                    }
                    QueryState::Idle | QueryState::Running { .. } | QueryState::Failed { .. } => {
                        None
                    }
                })
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        // The rows on screen are the previous run's until this one ends.
                        .when(self.is_running(), |grid| grid.opacity(0.35))
                        .child(DataTable::new(&self.results).stripe(true).bordered(false)),
                )
                .children(self.render_results_footer(cx))
                .into_any_element(),
        }
    }

    /// Above the grid: one button per result set and one for messages when there
    /// is more than one view, and Export CSV while a result with columns shows.
    fn render_result_switcher(&self, output: &QueryOutput, cx: &mut Context<Self>) -> AnyElement {
        let views = output.result_sets.len() + usize::from(!output.messages.is_empty());
        let exportable = !self.show_messages
            && self
                .shown_result
                .as_ref()
                .is_some_and(|result| !result.columns.is_empty());
        if views < 2 && !exportable {
            return div().into_any_element();
        }
        let export = exportable.then(|| {
            Button::new("export-csv")
                .ghost()
                .xsmall()
                .icon(LucideIcon::Download)
                .label("Export CSV")
                .tooltip("Save the result as CSV (Ctrl+Shift+E)")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.export_csv(&ExportCsv, window, cx);
                }))
        });
        let mut views_bar = h_flex().gap_1();
        let bar = h_flex()
            .justify_between()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border);
        for (index, result) in output.result_sets.iter().enumerate() {
            let selected = !self.show_messages && index == self.shown_index;
            let label = format!("Result {} · {} rows", index + 1, result.rows.len());
            let mut button = Button::new(("result-set", index))
                .xsmall()
                .label(label)
                .on_click(cx.listener(move |this, _, _, cx| this.select_result_set(index, cx)));
            button = if selected {
                button.primary()
            } else {
                button.ghost()
            };
            views_bar = views_bar.child(button);
        }
        if !output.messages.is_empty() {
            let label = format!("Messages · {}", output.messages.len());
            let mut button = Button::new("messages")
                .xsmall()
                .label(label)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.show_messages = true;
                    cx.notify();
                }));
            button = if self.show_messages {
                button.primary()
            } else {
                button.ghost()
            };
            views_bar = views_bar.child(button);
        }
        // A single view needs no switcher, only the export button.
        bar.child(if views < 2 { h_flex() } else { views_bar })
            .children(export)
            .into_any_element()
    }

    pub fn status_text(&self) -> String {
        let status = self.query_status_text();
        match &self.export_notice {
            Some(notice) => format!("{status} · {notice}"),
            None => status,
        }
    }

    fn query_status_text(&self) -> String {
        match &self.state {
            QueryState::Idle => "Ready".to_owned(),
            QueryState::Running { started, .. } => {
                format!("Running… {:.1}s", started.elapsed().as_secs_f64())
            }
            QueryState::Failed { .. } => "Failed".to_owned(),
            QueryState::Done { output, elapsed } => {
                let mut parts = Vec::new();
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
                if let Some(message) = output.messages.last() {
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
            .on_action(cx.listener(Self::format_sql))
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

fn report_export(this: &WeakEntity<QueryTab>, notice: String, cx: &mut AsyncApp) {
    let updated = this.update(cx, |tab, cx| {
        tab.export_notice = Some(notice.into());
        cx.notify();
    });
    if let Err(error) = updated {
        tracing::debug!(%error, "tab closed before the export finished");
    }
}

fn write_csv_file(path: &Path, result_set: &ResultSet) -> anyhow::Result<()> {
    let file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    sql_db::write_csv(result_set, std::io::BufWriter::new(file))
        .with_context(|| format!("writing {}", path.display()))
}

/// Downloads, else home, else the working directory.
fn export_directory() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|directories| {
            directories
                .download_dir()
                .map(Path::to_path_buf)
                .or_else(|| Some(directories.home_dir().to_path_buf()))
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

fn export_file_name(tab_number: usize, database: &str) -> String {
    let stem: String = database
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if stem.is_empty() {
        format!("query-{tab_number}.csv")
    } else {
        format!("{stem}-query-{tab_number}.csv")
    }
}

/// "line 3" or "lines 3–5", 1-based, for the byte range that ran.
fn line_span(text: &str, range: &Range<usize>) -> SharedString {
    let line_of = |offset: usize| {
        text.get(..offset)
            .map_or(0, |before| before.matches('\n').count())
            + 1
    };
    let first = line_of(range.start);
    let last = line_of(range.end);
    if first == last {
        format!("line {first}").into()
    } else {
        format!("lines {first}–{last}").into()
    }
}

/// The URI a language server knows this tab's text by.
fn tab_document(number: usize) -> Option<lsp_types::Uri> {
    format!("file:///sql-editor/tab-{number}.sql").parse().ok()
}

/// `text` with LSP edits applied; edits are applied from the end so earlier offsets stay valid.
fn apply_text_edits(text: &str, edits: &[lsp_types::TextEdit]) -> String {
    let rope = Rope::from(text);
    let mut ranges: Vec<(usize, Range<usize>, &str)> = edits
        .iter()
        .enumerate()
        .map(|(index, edit)| {
            (
                index,
                rope.position_to_offset(&edit.range.start)
                    ..rope.position_to_offset(&edit.range.end),
                edit.new_text.as_str(),
            )
        })
        .collect();
    // From the end; inserts at one position go in reverse so they read in array order.
    ranges.sort_by_key(|(index, range, _)| std::cmp::Reverse((range.start, *index)));
    let mut result = text.to_owned();
    for (_, range, new_text) in ranges {
        if result.is_char_boundary(range.start)
            && result.is_char_boundary(range.end)
            && range.start <= range.end
        {
            result.replace_range(range, new_text);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_file_name_is_safe() {
        assert_eq!(export_file_name(2, ""), "query-2.csv");
        assert_eq!(
            export_file_name(3, "dbo/sales data"),
            "dbo_sales_data-query-3.csv"
        );
    }

    #[test]
    fn describes_line_span() {
        let text = "select 1;\nselect\n  2;";
        assert_eq!(line_span(text, &(0..8)), "line 1");
        assert_eq!(line_span(text, &(10..20)), "lines 2–3");
    }

    #[test]
    fn applies_text_edits_from_the_end() {
        let edit = |line, start, end, text: &str| lsp_types::TextEdit {
            range: lsp_types::Range::new(
                lsp_types::Position::new(line, start),
                lsp_types::Position::new(line, end),
            ),
            new_text: text.to_owned(),
        };
        let edits = [edit(0, 0, 6, "SELECT"), edit(1, 0, 4, "FROM")];
        assert_eq!(
            apply_text_edits("select 1\nfrom t", &edits),
            "SELECT 1\nFROM t"
        );
        let inserts = [edit(0, 0, 0, "a"), edit(0, 0, 0, "b")];
        assert_eq!(apply_text_edits("x", &inserts), "abx");
    }
}
