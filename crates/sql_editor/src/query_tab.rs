use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputState};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, h_flex, resizable_panel, v_flex, v_resizable,
};
use gpui_kit::{
    App, AppContext as _, AsyncApp, Context, Entity, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Task, WeakEntity, Window, actions, div, px,
};
use sql_db::{CancelHandle, FabricConfig, QueryError, QueryOutput, ResultSet, TdsConnection};
use tokio::sync::Mutex;

use crate::KEY_CONTEXT;
use crate::results_table::ResultsTable;
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
        ExportCsv
    ]
);

const HOST_ENV: &str = "SQL_EDITOR_FABRIC_HOST";
const DATABASE_ENV: &str = "SQL_EDITOR_FABRIC_DATABASE";

type SharedConnection = Arc<Mutex<TdsConnection>>;
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

/// Connection fields a new tab starts with.
#[derive(Debug, Clone, Default)]
pub struct ConnectionFields {
    pub host: String,
    pub database: String,
}

impl ConnectionFields {
    pub fn from_env() -> Self {
        Self {
            host: std::env::var(HOST_ENV).unwrap_or_default(),
            database: std::env::var(DATABASE_ENV).unwrap_or_default(),
        }
    }
}

pub struct QueryTab {
    number: usize,
    focus_handle: FocusHandle,
    host_input: Entity<InputState>,
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
}

impl QueryTab {
    pub fn new(
        number: usize,
        fields: ConnectionFields,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let host_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("xxxx.datawarehouse.fabric.microsoft.com")
                .default_value(fields.host)
        });
        let database_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("warehouse or lakehouse")
                .default_value(fields.database)
        });
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql".to_owned())
                .line_number(true)
                .soft_wrap(false)
                .placeholder("select top 100 * from ...")
        });
        let results = cx.new(|cx| TableState::new(ResultsTable::default(), window, cx));

        Self {
            number,
            focus_handle: cx.focus_handle(),
            host_input,
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
        }
    }

    pub fn connection_fields(&self, cx: &App) -> ConnectionFields {
        ConnectionFields {
            host: self.host_input.read(cx).value().trim().to_owned(),
            database: self.database_input.read(cx).value().trim().to_owned(),
        }
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

    fn connection_config(&self, cx: &Context<Self>) -> Result<FabricConfig, SharedString> {
        let ConnectionFields { host, database } = self.connection_fields(cx);
        if host.is_empty() || database.is_empty() {
            return Err("Enter the SQL endpoint host and the warehouse or lakehouse name.".into());
        }
        Ok(FabricConfig::new(host, database))
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
                .is_ok_and(|connection| *connection.config() == config)
        });
        let cancel = CancelHandle::new();
        let query = tokio_bridge::spawn(cx, {
            let cancel = cancel.clone();
            async move {
                let connection = match reusable {
                    Some(connection) => connection,
                    None => Arc::new(Mutex::new(TdsConnection::connect_fabric(config).await?)),
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
        let exportable = self
            .shown_result
            .as_ref()
            .is_some_and(|result| !result.columns.is_empty());
        h_flex()
            .gap_2()
            .p_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .w(px(420.))
                    .child(Input::new(&self.host_input).small()),
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
            .child(
                Button::new("export-csv")
                    .small()
                    .label("Export CSV")
                    .tooltip("Save the result as CSV (Ctrl+Shift+E)")
                    .disabled(!exportable)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.export_csv(&ExportCsv, window, cx);
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
            QueryState::Idle | QueryState::Running { .. } | QueryState::Done { .. } => div()
                .size_full()
                .child(DataTable::new(&self.results).stripe(true).bordered(false))
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
