//! Completion and hover for the SQL editor, from the schema cache and
//! `sql_text::completion`'s reading of the statement under the cursor.

use std::sync::Arc;

use anyhow::Result;
use gpui_kit::component::input::{CompletionProvider, HoverProvider, Rope, RopeExt as _};
use gpui_kit::component::select::SelectState;
use gpui_kit::{App, SharedString, Task, WeakEntity, Window};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit,
    Hover, HoverContents, MarkupContent, MarkupKind, TextEdit,
};
use sql_db::{ColumnInfo, Dialect};
use sql_text::completion::{self, Expectation, TableRef};

use crate::lsp_servers;
use crate::schema_cache::{self, Schema};
use crate::tokio_bridge;

/// Most items shown at once; the menu narrows as the user types.
const MAX_ITEMS: usize = 200;

/// The tab's connection and database fields. The providers read these, not
/// the tab: they run inside editor updates the tab itself may be making.
#[derive(Clone)]
pub struct TabTarget {
    pub connection: WeakEntity<SelectState<Vec<SharedString>>>,
    pub database: WeakEntity<crate::database_picker::DatabaseSelect>,
    /// The tab's document for a language server.
    pub document: Option<lsp_types::Uri>,
}

impl TabTarget {
    fn connection_name(&self, cx: &App) -> Option<SharedString> {
        self.connection
            .upgrade()?
            .read(cx)
            .selected_value()
            .cloned()
    }

    /// The connection's language server, with this tab's text sent to it.
    fn server(
        &self,
        cx: &mut App,
        text: &str,
    ) -> Option<(Arc<sql_lsp::LspClient>, lsp_types::Uri)> {
        let connection = self.connection_name(cx)?;
        let document = self.document.clone()?;
        let client = lsp_servers::client(cx, &connection)?;
        lsp_servers::sync_document(cx, &connection, &document, text);
        Some((client, document))
    }
}

/// How long completion waits for the server before showing built-in items alone.
const SERVER_COMPLETION_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

pub struct SqlCompletions {
    pub target: TabTarget,
}

pub struct SqlHover {
    pub target: TabTarget,
}

/// The schema for the tab's connection and database, when loaded.
fn tab_schema(target: &TabTarget, cx: &mut App) -> Option<Arc<Schema>> {
    let connection = target.connection_name(cx)?;
    let database = target
        .database
        .upgrade()?
        .read(cx)
        .selected_value()
        .map(|database| database.trim().to_owned())
        .unwrap_or_default();
    let database = match database.as_str() {
        "" => crate::connections::find(cx, &connection)
            .filter(|saved| saved.settings.driver.is_file())
            .map(|_| "main".to_owned())?,
        database => database.to_owned(),
    };
    schema_cache::get(cx, &connection, &database)
}

impl CompletionProvider for SqlCompletions {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let source = text.to_string();
        let context = completion::context_at(&source, offset);
        let schema = tab_schema(&self.target, cx);
        let dialect = schema
            .as_ref()
            .and_then(|schema| schema.dialect)
            .unwrap_or(Dialect::TSql);
        let candidates = candidates(&context, schema.as_deref(), dialect);
        let range = text.offset_to_position(context.replace.start)
            ..text.offset_to_position(context.replace.end);
        let prefix = context.prefix.to_lowercase();
        let mut matching: Vec<Candidate> = candidates
            .into_iter()
            .filter(|candidate| {
                candidate
                    .label
                    .to_lowercase()
                    .trim_start_matches(['[', '"'])
                    .starts_with(&prefix)
            })
            .collect();
        matching.sort_by(|left, right| {
            left.rank
                .cmp(&right.rank)
                .then_with(|| left.label.cmp(&right.label))
        });
        matching.dedup_by(|left, right| left.label == right.label);
        let items = matching
            .into_iter()
            .take(MAX_ITEMS)
            .enumerate()
            .map(|(index, candidate)| CompletionItem {
                label: candidate.label.clone(),
                kind: Some(candidate.kind),
                detail: Some(candidate.detail),
                sort_text: Some(format!("{index:04}")),
                filter_text: Some(candidate.label.clone()),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: lsp_types::Range {
                        start: range.start,
                        end: range.end,
                    },
                    new_text: candidate.label,
                })),
                ..CompletionItem::default()
            })
            .collect();
        let server = self
            .target
            .server(cx, &source)
            .filter(|(client, _)| client.capabilities().completion_provider.is_some());
        let Some((client, document)) = server else {
            return Task::ready(Ok(CompletionResponse::Array(items)));
        };
        let params = lsp_types::CompletionParams {
            text_document_position: lsp_types::TextDocumentPositionParams::new(
                lsp_types::TextDocumentIdentifier::new(document),
                text.offset_to_position(offset),
            ),
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
            context: None,
        };
        let request = tokio_bridge::spawn(cx, async move {
            tokio::time::timeout(
                SERVER_COMPLETION_TIMEOUT,
                client.request::<lsp_types::request::Completion>(params),
            )
            .await
        });
        cx.spawn(async move |_| {
            let server_items = match request.await {
                Ok(Ok(Ok(Some(CompletionResponse::Array(items))))) => items,
                Ok(Ok(Ok(Some(CompletionResponse::List(list))))) => list.items,
                Ok(Ok(Ok(None)) | Err(_)) => Vec::new(),
                Ok(Ok(Err(error))) => {
                    tracing::debug!(error = error.kind(), "server completion");
                    Vec::new()
                }
                Err(error) => {
                    tracing::debug!(%error, "server completion task");
                    Vec::new()
                }
            };
            Ok(CompletionResponse::Array(merge(items, server_items)))
        })
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        !new_text.is_empty()
            && new_text
                .chars()
                .all(|character| character.is_alphanumeric() || matches!(character, '_' | '.'))
    }
}

/// Every name or keyword that fits `context`, before filtering by the prefix.
fn candidates(
    context: &completion::CompletionContext,
    schema: Option<&Schema>,
    dialect: Dialect,
) -> Vec<Candidate> {
    let mut candidates: Vec<Candidate> = Vec::new();
    // A warehouse has thousands of names; only those the prefix fits are built.
    let prefix = context.prefix.to_lowercase();
    match (&context.expectation, schema) {
        (Expectation::Table { schema: wanted }, Some(schema)) => {
            for object in &schema.objects {
                if has_prefix(&object.name, &prefix)
                    && wanted
                        .as_deref()
                        .is_none_or(|wanted| object.schema.eq_ignore_ascii_case(wanted))
                {
                    candidates.push(Candidate::named(
                        dialect.name_in_sql(&object.name),
                        CompletionItemKind::CLASS,
                        format!(
                            "{} {}",
                            object.schema,
                            object.kind.plural_label().to_lowercase()
                        ),
                    ));
                }
            }
            if wanted.is_none() {
                for name in schema.schemas() {
                    candidates.push(Candidate::named(
                        dialect.name_in_sql(name),
                        CompletionItemKind::MODULE,
                        "schema".to_owned(),
                    ));
                }
            }
        }
        (
            Expectation::Column {
                qualifier: Some(qualifier),
            },
            Some(schema),
        ) => {
            let table = context
                .tables
                .iter()
                .find(|table| table.is_called(qualifier));
            let columns = table
                .and_then(|table| schema.resolve(table))
                .map(|(_, columns)| columns);
            for column in columns.unwrap_or_default() {
                candidates.push(column_candidate(dialect, column, None));
            }
        }
        (Expectation::Column { qualifier: None }, Some(schema)) => {
            for table in &context.tables {
                let name = table.alias.as_deref().unwrap_or(&table.name);
                candidates.push(Candidate::named(
                    dialect.name_in_sql(name),
                    CompletionItemKind::VARIABLE,
                    format!(
                        "{} {}",
                        if table.alias.is_some() {
                            "alias of"
                        } else {
                            "table"
                        },
                        table.name
                    ),
                ));
                if let Some((_, columns)) = schema.resolve(table) {
                    for column in columns {
                        if has_prefix(&column.name, &prefix) {
                            candidates.push(column_candidate(dialect, column, Some(name)));
                        }
                    }
                }
            }
            keyword_candidates(&mut candidates, &context.prefix);
        }
        (Expectation::Table { .. } | Expectation::Column { .. }, None)
        | (Expectation::Keyword, _) => {
            keyword_candidates(&mut candidates, &context.prefix);
        }
    }
    candidates
}

/// Whether `name` starts with `lowered_prefix`, ignoring case, without allocating.
fn has_prefix(name: &str, lowered_prefix: &str) -> bool {
    let mut characters = name.chars().flat_map(char::to_lowercase);
    lowered_prefix
        .chars()
        .all(|expected| characters.next() == Some(expected))
}

/// Built-in items first, then the server's, skipping labels already offered.
fn merge(mut items: Vec<CompletionItem>, server_items: Vec<CompletionItem>) -> Vec<CompletionItem> {
    let mut seen: std::collections::HashSet<String> =
        items.iter().map(|item| item.label.to_lowercase()).collect();
    let offset = items.len();
    for (index, mut item) in server_items.into_iter().enumerate() {
        if seen.insert(item.label.to_lowercase()) {
            item.sort_text = Some(format!("{:04}", offset + index));
            items.push(item);
        }
    }
    items
}

struct Candidate {
    label: String,
    kind: CompletionItemKind,
    detail: String,
    /// Columns first, then tables and aliases, then keywords.
    rank: u8,
}

impl Candidate {
    fn named(label: String, kind: CompletionItemKind, detail: String) -> Self {
        let rank = match kind {
            CompletionItemKind::FIELD => 0,
            CompletionItemKind::KEYWORD => 2,
            _ => 1,
        };
        Self {
            label,
            kind,
            detail,
            rank,
        }
    }
}

fn column_candidate(dialect: Dialect, column: &ColumnInfo, table: Option<&str>) -> Candidate {
    let null = if column.nullable { "null" } else { "not null" };
    let detail = match table {
        Some(table) => format!("{table}: {} {null}", column.type_name),
        None => format!("{} {null}", column.type_name),
    };
    Candidate::named(
        dialect.name_in_sql(&column.name),
        CompletionItemKind::FIELD,
        detail,
    )
}

fn keyword_candidates(candidates: &mut Vec<Candidate>, prefix: &str) {
    // Keywords follow the case the user started typing in.
    let upper = !prefix.is_empty() && prefix.chars().all(|character| !character.is_lowercase());
    for keyword in completion::KEYWORDS {
        let label = if upper {
            keyword.to_uppercase()
        } else {
            (*keyword).to_owned()
        };
        candidates.push(Candidate::named(
            label,
            CompletionItemKind::KEYWORD,
            "keyword".to_owned(),
        ));
    }
}

impl HoverProvider for SqlHover {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Option<Hover>>> {
        let Some(schema) = tab_schema(&self.target, cx) else {
            return server_hover(&self.target, text, offset, cx);
        };
        let source = text.to_string();
        let markdown = completion::name_at(&source, offset).and_then(|(parts, _)| {
            describe(&schema, &completion::tables_at(&source, offset), &parts)
        });
        if let Some(value) = markdown {
            return Task::ready(Ok(Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value,
                }),
                range: None,
            })));
        }
        server_hover(&self.target, text, offset, cx)
    }
}

/// The language server's hover, when the built-in one has nothing.
fn server_hover(
    target: &TabTarget,
    text: &Rope,
    offset: usize,
    cx: &mut App,
) -> Task<Result<Option<Hover>>> {
    let source = text.to_string();
    let server = target
        .server(cx, &source)
        .filter(|(client, _)| lsp_servers::can_hover(client.capabilities()));
    let Some((client, document)) = server else {
        return Task::ready(Ok(None));
    };
    let params = lsp_types::HoverParams {
        text_document_position_params: lsp_types::TextDocumentPositionParams::new(
            lsp_types::TextDocumentIdentifier::new(document),
            text.offset_to_position(offset),
        ),
        work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
    };
    let request = tokio_bridge::spawn(cx, async move {
        client
            .request::<lsp_types::request::HoverRequest>(params)
            .await
    });
    cx.spawn(async move |_| match request.await {
        Ok(Ok(hover)) => Ok(hover),
        Ok(Err(error)) => {
            tracing::debug!(error = error.kind(), "server hover");
            Ok(None)
        }
        Err(error) => {
            tracing::debug!(%error, "server hover task");
            Ok(None)
        }
    })
}

/// Markdown for the table or column `parts` names, if the schema knows it.
fn describe(schema: &Schema, tables: &[TableRef], parts: &[String]) -> Option<String> {
    let column_text = |table: &str, column: &ColumnInfo| {
        let null = if column.nullable { "null" } else { "not null" };
        format!(
            "**{}** `{}` {null}\n\ncolumn of {table}",
            column.name, column.type_name
        )
    };
    let table_text = |object: &sql_db::CatalogObject, columns: &[ColumnInfo]| {
        let mut lines = vec![format!(
            "**{}.{}** ({} columns)",
            object.schema,
            object.name,
            columns.len()
        )];
        lines.extend(
            columns
                .iter()
                .take(30)
                .map(|column| format!("- {} `{}`", column.name, column.type_name)),
        );
        lines.join("\n")
    };
    match parts {
        [qualifier, name] => {
            if let Some(table) = tables.iter().find(|table| table.is_called(qualifier)) {
                let (object, columns) = schema.resolve(table)?;
                let column = columns
                    .iter()
                    .find(|column| column.name.eq_ignore_ascii_case(name))?;
                return Some(column_text(
                    &format!("{}.{}", object.schema, object.name),
                    column,
                ));
            }
            let reference = TableRef {
                schema: Some(qualifier.clone()),
                name: name.clone(),
                alias: None,
            };
            let (object, columns) = schema.resolve(&reference)?;
            Some(table_text(object, columns))
        }
        [name] => {
            let as_table = tables
                .iter()
                .find(|table| table.is_called(name) || table.name.eq_ignore_ascii_case(name))
                .cloned()
                .unwrap_or_else(|| TableRef {
                    schema: None,
                    name: name.clone(),
                    alias: None,
                });
            if let Some((object, columns)) = schema.resolve(&as_table) {
                return Some(table_text(object, columns));
            }
            tables.iter().find_map(|table| {
                let (object, columns) = schema.resolve(table)?;
                let column = columns
                    .iter()
                    .find(|column| column.name.eq_ignore_ascii_case(name))?;
                Some(column_text(
                    &format!("{}.{}", object.schema, object.name),
                    column,
                ))
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use sql_db::{CatalogObject, ObjectKind, SchemaColumn};

    use super::*;

    fn schema() -> Schema {
        let column = |table: &str, name: &str| SchemaColumn {
            schema: "dbo".to_owned(),
            table: table.to_owned(),
            column: ColumnInfo {
                name: name.to_owned(),
                type_name: "int".to_owned(),
                nullable: true,
            },
        };
        Schema::new(
            Dialect::TSql,
            vec![
                CatalogObject {
                    schema: "dbo".to_owned(),
                    name: "customers".to_owned(),
                    kind: ObjectKind::Table,
                },
                CatalogObject {
                    schema: "dbo".to_owned(),
                    name: "orders".to_owned(),
                    kind: ObjectKind::Table,
                },
            ],
            vec![
                column("customers", "id"),
                column("customers", "name"),
                column("orders", "customer_id"),
            ],
        )
    }

    fn labels(text: &str, cursor: usize) -> Vec<String> {
        let context = completion::context_at(text, cursor);
        candidates(&context, Some(&schema()), Dialect::TSql)
            .into_iter()
            .map(|candidate| candidate.label)
            .collect()
    }

    #[test]
    fn merges_server_items_after_built_in_ones() {
        let item = |label: &str| CompletionItem {
            label: label.to_owned(),
            ..CompletionItem::default()
        };
        let merged = merge(
            vec![item("customers"), item("orders")],
            vec![item("Orders"), item("payments")],
        );
        let labels: Vec<&str> = merged.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, vec!["customers", "orders", "payments"]);
        assert_eq!(merged[2].sort_text.as_deref(), Some("0003"));
    }

    #[test]
    fn matches_prefixes_without_case() {
        assert!(has_prefix("Customers", "cust"));
        assert!(has_prefix("ÉTAT", "ét"));
        assert!(has_prefix("anything", ""));
        assert!(!has_prefix("cu", "cust"));
        assert!(!has_prefix("orders", "cust"));
    }

    #[test]
    fn lists_the_columns_of_an_alias() {
        let text = "select c. from customers c";
        assert_eq!(labels(text, "select c.".len()), vec!["id", "name"]);
    }

    #[test]
    fn lists_tables_after_from() {
        let found = labels("select * from ", 14);
        assert!(found.contains(&"customers".to_owned()), "{found:?}");
        assert!(found.contains(&"dbo".to_owned()), "{found:?}");
    }

    #[test]
    fn describes_columns_and_tables_on_hover() {
        let tables = completion::tables_at("select c.name from customers c", 0);
        let column = describe(&schema(), &tables, &["c".to_owned(), "name".to_owned()]).unwrap();
        assert!(column.contains("**name**"), "{column}");
        let table = describe(&schema(), &tables, &["customers".to_owned()]).unwrap();
        assert!(table.contains("dbo.customers"), "{table}");
    }
}
