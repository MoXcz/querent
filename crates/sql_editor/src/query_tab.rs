use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use gpui_kit::base::input::{Diagnostic, DiagnosticSeverity};
use gpui_kit::component::IndexPath;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{
    Editor, EditorState, Input, InputEvent, InputState, RangeDecoration, RangeDecorationCollection,
    RangeDecorationStyle, Redo, Rope, RopeExt as _, Undo,
};
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, h_flex, resizable_panel, v_flex, v_resizable,
};
use gpui_kit::{
    App, AppContext as _, AsyncApp, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, Keystroke, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Task, WeakEntity, Window, actions, div, px,
};
use sql_db::{
    CancelHandle, Connection, ConnectionConfig, Dialect, QueryError, QueryOutput, ResultSet,
};
use sql_text::vim::{Buffer, Edit, Key, Mode, Vim};
use tokio::sync::Mutex;

use crate::KEY_CONTEXT;
use crate::connections::{self, ConnectionStore};
use crate::results_table::ResultsTable;
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
        ToggleVimMode
    ]
);

type SharedConnection = Arc<Mutex<Connection>>;

const BLOCK_CURSOR_OPACITY: f32 = 0.45;
const RUN_HIGHLIGHT_DURATION: Duration = Duration::from_millis(600);
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
    },
    Done {
        output: QueryOutput,
        elapsed: Duration,
    },
    Failed {
        message: SharedString,
    },
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
    database_input: Entity<InputState>,
    editor: Entity<EditorState>,
    results: Entity<TableState<ResultsTable>>,
    connection: Option<SharedConnection>,
    state: QueryState,
    running_query: Option<Task<()>>,
    /// Bumped per run so a replaced query's late result is ignored.
    query_generation: u64,
    /// Which lines the last run sent, shown in the status line.
    ran_lines: Option<SharedString>,
    /// The result set in the grid; shared with the grid and CSV export.
    shown_result: Option<Arc<ResultSet>>,
    /// Outcome of the last export, shown in the status line until the next run.
    export_notice: Option<SharedString>,
    export_task: Option<Task<()>>,
    vim: Vim,
    /// Fill under the character at the cursor in normal and visual mode.
    block_cursor: Option<RangeDecorationCollection>,
    block_cursor_range: Option<Range<usize>>,
    /// Brief fill over the SQL that was sent to the server.
    run_highlight: Option<RangeDecorationCollection>,
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
        let database_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("database")
                .default_value(fields.database)
        });
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql".to_owned())
                .line_number(true)
                .soft_wrap(false)
                .placeholder("select top 100 * from ...")
        });
        let target = TabTarget {
            connection: connection_select.downgrade(),
            database: database_input.downgrade(),
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
                    tab.handle_vim_keystroke(&event.keystroke, window, cx)
                });
                if handled {
                    cx.stop_propagation();
                }
            }
        });

        let subscriptions = vec![
            vim_interceptor,
            cx.subscribe_in(&connection_select, window, Self::on_connection_selected),
            cx.observe_global_in::<ConnectionStore>(window, Self::on_connections_changed),
            // The editor notifies on every cursor move and caret blink.
            cx.observe(&editor, |this, _, cx| this.update_block_cursor(cx)),
            cx.observe_global::<SettingsStore>(Self::update_block_cursor),
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
            database_input,
            editor,
            results,
            connection: None,
            state: QueryState::Idle,
            running_query: None,
            query_generation: 0,
            ran_lines: None,
            shown_result: None,
            export_notice: None,
            export_task: None,
            vim: Vim::default(),
            block_cursor: None,
            block_cursor_range: None,
            run_highlight: None,
            run_highlight_task: None,
            syntax_check: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn connection_fields(&self, cx: &App) -> ConnectionFields {
        ConnectionFields {
            connection: self.connection_select.read(cx).selected_value().cloned(),
            database: self.database_input.read(cx).value().trim().to_owned(),
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
        self.database_input.update(cx, |input, cx| {
            input.set_value(database.to_owned(), window, cx);
        });
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
        let database = connections::find(cx, name)
            .and_then(|connection| connection.config().ok())
            .map(|config| config.database().to_owned())
            .filter(|database| !database.is_empty());
        if let Some(database) = database {
            self.database_input.update(cx, |input, cx| {
                input.set_value(database, window, cx);
            });
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
        let rope = Rope::from(text.as_str());
        self.editor.update(cx, |editor, cx| {
            let Some(diagnostics) = editor.diagnostics_mut() else {
                return;
            };
            diagnostics.reset(&rope);
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
        let database = self.database_input.read(cx).value();
        let database = database.trim();
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

    fn start_query(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_running_query(cx);
        self.export_notice = None;
        let config = match self.connection_config(cx) {
            Ok(config) => config,
            Err(message) => {
                self.show_error(message, cx);
                return;
            }
        };

        let reusable = self.connection.clone().filter(|connection| {
            connection
                .try_lock()
                .is_ok_and(|connection| connection.matches(&config))
        });
        let cancel = CancelHandle::new();
        let query = tokio_bridge::spawn(cx, {
            let cancel = cancel.clone();
            async move {
                let connection = match reusable {
                    Some(connection) => connection,
                    None => Arc::new(Mutex::new(Connection::connect(config).await?)),
                };
                let output = connection.lock().await.execute(&sql, &cancel).await;
                Ok::<_, QueryError>((connection, output))
            }
        });

        self.state = QueryState::Running {
            started: Instant::now(),
            cancel,
        };
        self.query_generation += 1;
        let generation = self.query_generation;
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
        let elapsed = match &self.state {
            QueryState::Running { started, .. } => started.elapsed(),
            QueryState::Idle | QueryState::Done { .. } | QueryState::Failed { .. } => {
                Duration::ZERO
            }
        };
        self.running_query = None;
        self.state = match outcome {
            Ok(Ok((connection, Ok(output)))) => {
                self.connection = Some(connection);
                let shown = Arc::new(output.result_sets.last().cloned().unwrap_or_default());
                self.shown_result = Some(shown.clone());
                self.results.update(cx, |table, cx| {
                    table.delegate_mut().set_result(shown);
                    table.refresh(cx);
                    cx.notify();
                });
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
            .child(
                div()
                    .w(px(220.))
                    .child(Input::new(&self.database_input).small()),
            )
            .child(
                Button::new("run")
                    .primary()
                    .small()
                    .label("Run")
                    .tooltip("Run statement under cursor or selection (Ctrl+Enter). Run all: Ctrl+Shift+Enter")
                    .disabled(running)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.run_query(&RunQuery, window, cx)),
                    ),
            )
            .child(
                Button::new("cancel")
                    .small()
                    .label("Cancel")
                    .disabled(!running)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.cancel_query(&CancelQuery, window, cx);
                    })),
            )
    }

    /// Row count and Export CSV under the grid, shown while it holds a result.
    fn render_results_footer(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let result = self
            .shown_result
            .as_ref()
            .filter(|result| !result.columns.is_empty())?;
        let mut summary = format!("{} rows", result.rows.len());
        if result.truncated {
            summary.push_str(" (truncated)");
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
                .child(
                    Button::new("export-csv")
                        .ghost()
                        .xsmall()
                        .label("Export CSV")
                        .tooltip("Save the result as CSV (Ctrl+Shift+E)")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.export_csv(&ExportCsv, window, cx);
                        })),
                ),
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
            QueryState::Idle | QueryState::Running { .. } | QueryState::Done { .. } => v_flex()
                .size_full()
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .child(DataTable::new(&self.results).stripe(true).bordered(false)),
                )
                .children(self.render_results_footer(cx))
                .into_any_element(),
        }
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
            QueryState::Running { .. } => "Running…".to_owned(),
            QueryState::Failed { .. } => "Failed".to_owned(),
            QueryState::Done { output, elapsed } => {
                let mut parts = Vec::new();
                if let Some(ran_lines) = &self.ran_lines {
                    parts.push(ran_lines.to_string());
                }
                if let Some(result_set) = output.result_sets.last() {
                    parts.push(format!("{} rows", result_set.rows.len()));
                    if result_set.truncated {
                        parts.push("truncated".to_owned());
                    }
                }
                if output.result_sets.len() > 1 {
                    parts.push(format!(
                        "showing last of {} result sets",
                        output.result_sets.len()
                    ));
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
}
