//! Syntax checks, language-server documents and formatting for a tab.

use std::ops::Range;
use std::time::Duration;

use gpui_kit::base::input::{Diagnostic, DiagnosticSeverity};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::{Rope, RopeExt as _};
use gpui_kit::{App, Context, Window};
use sql_db::Dialect;
use sql_text::format::FormatDialect;

use super::{FormatSql, QueryTab, ServerView};
use crate::connections;
use crate::lsp_servers;
use crate::settings;
use crate::tokio_bridge;

/// Pause after typing before the syntax is checked again.
const SYNTAX_CHECK_DELAY: Duration = Duration::from_millis(400);
const SYNTAX_CHECK_MAX_BYTES: usize = 256 * 1024;

impl QueryTab {
    /// The SQL dialect of the tab's connection, if it has one.
    pub(super) fn dialect(&self, cx: &App) -> Option<Dialect> {
        let connection = self.connection_fields(cx).connection?;
        connections::find(cx, &connection).map(connections::SavedConnection::dialect)
    }

    pub(super) fn schedule_syntax_check(&mut self, cx: &mut Context<Self>) {
        self.syntax_check = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SYNTAX_CHECK_DELAY).await;
            if let Err(error) = this.update(cx, Self::check_syntax) {
                tracing::debug!(%error, "tab closed before the syntax check");
            }
        }));
    }

    /// Whether this tab's language server runs, and its diagnostics for the tab.
    pub(super) fn server_view(&self, cx: &App) -> ServerView {
        let (Some(connection), Some(document)) =
            (self.connection_fields(cx).connection, &self.document)
        else {
            return ServerView::default();
        };
        let running = matches!(
            lsp_servers::status(cx, &connection),
            Some((_, lsp_servers::ServerStatus::Running))
        );
        (
            running,
            lsp_servers::diagnostics(cx, &connection, document.as_str()),
        )
    }

    /// Every tab hears every server change; only one that concerns this tab rechecks.
    pub(super) fn on_servers_changed(&mut self, cx: &mut Context<Self>) {
        if self.server_view(cx) != self.seen_server {
            self.check_syntax(cx);
        }
    }

    /// Marks syntax errors in the editor. T-SQL is skipped: the grammar does
    /// not know `TOP`, `[names]`, `GO` or `DECLARE` and would flag valid code.
    pub(super) fn check_syntax(&mut self, cx: &mut Context<Self>) {
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
        self.seen_server = self.server_view(cx);
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
    pub(super) fn format_sql(
        &mut self,
        _: &FormatSql,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if settings::get(cx).format_with_language_server {
            self.format_with_server(window, cx);
        } else {
            self.format_built_in(window, cx);
        }
    }

    /// Formats the selection, or the whole buffer, as one undoable edit.
    pub(super) fn format_built_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn format_with_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            let failure = match request.await {
                Ok(Ok(edits)) => Ok(edits.unwrap_or_default()),
                Ok(Err(error)) => Err(error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            let edits = match failure {
                Ok(edits) => edits,
                Err(error) => {
                    let shown = this.update_in(cx, |_, window, cx| {
                        window.push_notification(format!("Format SQL failed: {error}."), cx);
                    });
                    if let Err(error) = shown {
                        tracing::debug!(%error, "tab closed before formatting failed");
                    }
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
}

/// The URI a language server knows this tab's text by.
pub(super) fn tab_document(number: usize) -> Option<lsp_types::Uri> {
    format!("file:///querent/tab-{number}.sql").parse().ok()
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
