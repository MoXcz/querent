//! The results half of a tab: the grid, the result-set bar, messages and CSV export.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::table::DataTable;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, AsyncApp, Context, IntoElement, ParentElement as _, Styled as _, WeakEntity,
    Window, div, prelude::FluentBuilder as _,
};
use sql_db::{QueryOutput, ResultSet};

use super::{CancelQuery, ExportCsv, QueryState, QueryTab};

/// Messages drawn at once; a loop of `PRINT`s can produce far more.
const MAX_SHOWN_MESSAGES: usize = 1_000;

impl QueryTab {
    pub(super) fn show_result_set(
        &mut self,
        output: &QueryOutput,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        let shown = output.result_sets.get(index).cloned().unwrap_or_default();
        self.show_result(index, shown, cx);
    }

    pub(super) fn show_result(
        &mut self,
        index: usize,
        shown: Arc<ResultSet>,
        cx: &mut Context<Self>,
    ) {
        self.shown_index = index;
        self.shown_result = Some(shown.clone());
        self.results.update(cx, |table, cx| {
            table.delegate_mut().set_result(shown);
            table.refresh(cx);
            cx.notify();
        });
        cx.notify();
    }

    pub(super) fn select_result_set(&mut self, index: usize, cx: &mut Context<Self>) {
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
    pub(super) fn rerun_without_limit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(super) fn export_csv(&mut self, _: &ExportCsv, _: &mut Window, cx: &mut Context<Self>) {
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

    /// A spinner, the elapsed time and Cancel above the grid while a query runs.
    pub(super) fn render_running_banner(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let QueryState::Running {
            started,
            cancelling,
            ..
        } = &self.state
        else {
            return None;
        };
        let cancelling = *cancelling;
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
                .child(format!(
                    "{} {:.1}s",
                    if cancelling {
                        "Cancelling…"
                    } else {
                        "Running…"
                    },
                    started.elapsed().as_secs_f64()
                ))
                .child(div().flex_1())
                .child(
                    Button::new("cancel-running")
                        .xsmall()
                        .icon(LucideIcon::CircleStop)
                        .label("Cancel")
                        .disabled(cancelling)
                        .tooltip("Stop the query on the server (Ctrl+Shift+C)")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.cancel_query(&CancelQuery, window, cx);
                        })),
                ),
        )
    }

    /// Row count and the no-limit rerun under the grid, shown while it holds a result.
    pub(super) fn render_results_footer(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let result = self
            .shown_result
            .as_ref()
            .filter(|result| !result.columns.is_empty())?;
        let mut summary = format!("{} rows", result.rows.len());
        let truncated = result.truncated;
        if truncated {
            summary.push_str(
                " (first rows only: the row limit or the 1 GB result budget was reached)",
            );
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
                        .tooltip("Runs the same SQL again without the row limit")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.rerun_without_limit(window, cx);
                        }))
                })),
        )
    }

    pub(super) fn render_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.state {
            QueryState::Cancelled { elapsed } => div()
                .size_full()
                .p_3()
                .text_color(cx.theme().muted_foreground)
                .child(format!(
                    "Query cancelled after {:.1}s.",
                    elapsed.as_secs_f64()
                ))
                .into_any_element(),
            QueryState::Failed { message } => div()
                .size_full()
                .p_3()
                .text_color(cx.theme().danger)
                .child(message.clone())
                .into_any_element(),
            QueryState::Done { output, failed, .. } if output.result_sets.is_empty() && !failed => {
                div()
                    .size_full()
                    .p_3()
                    .text_color(cx.theme().muted_foreground)
                    .child(match output.rows_affected {
                        Some(count) => format!("{count} rows affected."),
                        None => "Statement completed.".to_owned(),
                    })
                    .into_any_element()
            }
            QueryState::Done { output, failed, .. } if self.show_messages => v_flex()
                .size_full()
                .child(self.render_result_switcher(output, cx))
                .child(
                    v_flex()
                        .flex_1()
                        .min_h_0()
                        .p_3()
                        .gap_1()
                        .text_sm()
                        .children(hidden_messages(output.messages.len()).map(|hidden| {
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("{hidden} earlier messages are not shown."))
                        }))
                        .children(
                            output
                                .messages
                                .iter()
                                .enumerate()
                                .skip(hidden_messages(output.messages.len()).unwrap_or(0))
                                .map(|(index, message)| {
                                    let is_error = *failed && index + 1 == output.messages.len();
                                    div()
                                        .when(is_error, |line| line.text_color(cx.theme().danger))
                                        .child(message.clone())
                                }),
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
                    QueryState::Idle
                    | QueryState::Running { .. }
                    | QueryState::Failed { .. }
                    | QueryState::Cancelled { .. } => None,
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
    pub(super) fn render_result_switcher(
        &self,
        output: &QueryOutput,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
}

/// How many of `count` messages are left out, the oldest first.
fn hidden_messages(count: usize) -> Option<usize> {
    count
        .checked_sub(MAX_SHOWN_MESSAGES)
        .filter(|hidden| *hidden > 0)
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
}
