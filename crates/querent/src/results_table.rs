use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, ClipboardItem, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use std::sync::Arc;

use sql_db::{Headers, ResultSet, Value};

/// "Copy all rows" is offered up to this many; beyond it, Export CSV is the tool.
const MAX_COPIED_ROWS: usize = 100_000;
/// And only while the rows are small enough to build as one string on the UI thread.
const MAX_COPIED_BYTES: usize = 32 * 1024 * 1024;
/// Characters of one value the row viewer shows.
const MAX_VIEWED_CHARACTERS: usize = 20_000;

const CHARACTER_WIDTH: f32 = 7.5;
const MIN_COLUMN_WIDTH: f32 = 60.;
const MAX_COLUMN_WIDTH: f32 = 420.;
/// Rows sampled to size columns; scanning all rows would stall large results.
const WIDTH_SAMPLE_ROWS: usize = 200;

#[derive(Default)]
pub struct ResultsTable {
    result: Arc<ResultSet>,
    columns: Vec<Column>,
    /// Rows the filter keeps, in the order the server sent them; `None` keeps every row.
    filtered: Option<Arc<Vec<usize>>>,
    sort: Option<(usize, ColumnSort)>,
    /// Row numbers of `result` as the grid shows them, after the filter and the sort; the rows are not copied.
    order: Option<Arc<Vec<usize>>>,
}

impl ResultsTable {
    pub fn set_result(&mut self, result: Arc<ResultSet>) {
        self.columns = result
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                let widest_value = result
                    .rows
                    .iter()
                    .take(WIDTH_SAMPLE_ROWS)
                    .filter_map(|row| row.get(index))
                    .map(|value| value.to_string().chars().count())
                    .max()
                    .unwrap_or(0);
                let characters = widest_value.max(column.name.chars().count()) + 2;
                #[expect(clippy::cast_precision_loss, reason = "character counts are small")]
                let width =
                    (characters as f32 * CHARACTER_WIDTH).clamp(MIN_COLUMN_WIDTH, MAX_COLUMN_WIDTH);
                let mut table_column = Column::new(
                    SharedString::from(format!("column-{index}")),
                    column.name.clone(),
                )
                .width(px(width))
                .sortable();
                if result
                    .rows
                    .first()
                    .and_then(|row| row.get(index))
                    .is_some_and(Value::is_numeric)
                {
                    table_column = table_column.text_right();
                }
                table_column
            })
            .collect();
        self.result = result;
        self.filtered = None;
        self.sort = None;
        self.order = None;
    }

    /// Keeps only `rows` of the result, or every row for `None`.
    pub fn set_filtered(&mut self, rows: Option<Vec<usize>>) {
        self.filtered = rows.map(Arc::new);
        self.order = shown_order(&self.result, self.filtered.as_ref(), self.sort);
    }

    /// The rows the grid shows, in its order; `None` when that is every row as it arrived.
    pub fn shown_rows(&self) -> Option<Arc<Vec<usize>>> {
        self.order.clone()
    }

    pub fn shown_count(&self) -> usize {
        self.order
            .as_ref()
            .map_or(self.result.rows.len(), |order| order.len())
    }

    /// The row of the result set that grid row `row_index` shows.
    fn source_row(&self, row_index: usize) -> usize {
        self.order
            .as_ref()
            .and_then(|order| order.get(row_index).copied())
            .unwrap_or(row_index)
    }

    fn value(&self, row_index: usize, column_index: usize) -> Option<&Value> {
        self.result
            .rows
            .get(self.source_row(row_index))
            .and_then(|row| row.get(column_index))
    }

    pub fn clear(&mut self) {
        self.set_result(Arc::default());
    }
}

impl TableDelegate for ResultsTable {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.shown_count()
    }

    fn column(&self, column_index: usize, _: &App) -> Column {
        self.columns
            .get(column_index)
            .cloned()
            .unwrap_or_else(|| Column::new("missing", ""))
    }

    fn render_td(
        &mut self,
        row_index: usize,
        column_index: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        match self.value(row_index, column_index) {
            None | Some(Value::Null) => div().text_color(cx.theme().muted_foreground).child("NULL"),
            Some(value) => div().child(single_line(&value.to_string())),
        }
    }

    fn cell_text(&self, row_index: usize, column_index: usize, _: &App) -> String {
        self.value(row_index, column_index)
            .map(ToString::to_string)
            .unwrap_or_default()
    }

    fn perform_sort(
        &mut self,
        column_index: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        self.sort = Some((column_index, sort));
        self.order = shown_order(&self.result, self.filtered.as_ref(), self.sort);
        // The highlighted grid row now shows another row of the result.
        let table = cx.entity();
        cx.defer(move |cx| table.update(cx, TableState::clear_selection));
    }

    fn context_menu(
        &mut self,
        row_index: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let source_row = self.source_row(row_index);
        let copy_row = |label: &'static str, headers: Headers| {
            let result = self.result.clone();
            PopupMenuItem::new(label).on_click(move |_, _, cx| {
                let text = sql_db::rows_as_tsv(&result, [source_row], headers);
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            })
        };
        let mut menu = menu
            .item(copy_row("Copy row", Headers::Omit))
            .item(copy_row("Copy row with headers", Headers::Include));
        if self.result.rows.len() <= MAX_COPIED_ROWS
            && self.result.estimated_bytes() <= MAX_COPIED_BYTES
        {
            let result = self.result.clone();
            let order = self.order.clone();
            menu = menu.item(PopupMenuItem::new("Copy all rows with headers").on_click(
                move |_, _, cx| {
                    let text = match &order {
                        Some(order) => {
                            sql_db::rows_as_tsv(&result, order.iter().copied(), Headers::Include)
                        }
                        None => {
                            sql_db::rows_as_tsv(&result, 0..result.rows.len(), Headers::Include)
                        }
                    };
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                },
            ));
        }
        let fields = row_fields(&self.result, source_row);
        let result = self.result.clone();
        menu.separator().item(
            PopupMenuItem::new("View row").on_click(move |_, window, cx| {
                let fields = fields.clone();
                let result = result.clone();
                window.open_dialog(cx, move |dialog, _, cx| {
                    dialog.title("Row").w(px(720.)).child(
                        div()
                            .id("row-values")
                            .max_h(px(520.))
                            .overflow_y_scroll()
                            .child(v_flex().gap_3().children(fields.iter().enumerate().map(
                                |(index, (name, value))| {
                                    let result = result.clone();
                                    v_flex()
                                        .gap_1()
                                        .child(
                                            h_flex()
                                                .gap_2()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(name.clone())
                                                .child(
                                                    Button::new(("copy-value", index))
                                                        .ghost()
                                                        .xsmall()
                                                        .label("Copy")
                                                        .tooltip("Copy the whole value")
                                                        .on_click(move |_, _, cx| {
                                                            let text = whole_value(
                                                                &result, source_row, index,
                                                            );
                                                            cx.write_to_clipboard(
                                                                ClipboardItem::new_string(text),
                                                            );
                                                        }),
                                                ),
                                        )
                                        .child(div().text_sm().child(value.clone()))
                                },
                            ))),
                    )
                });
            }),
        )
    }
}

/// The row numbers the grid shows: the filtered rows, ordered by the sorted
/// column. `None` shows every row as it arrived.
fn shown_order(
    result: &ResultSet,
    filtered: Option<&Arc<Vec<usize>>>,
    sort: Option<(usize, ColumnSort)>,
) -> Option<Arc<Vec<usize>>> {
    let (column_index, descending) = match sort {
        None | Some((_, ColumnSort::Default)) => return filtered.cloned(),
        Some((column_index, ColumnSort::Ascending)) => (column_index, false),
        Some((column_index, ColumnSort::Descending)) => (column_index, true),
    };
    let value = |row: usize| result.rows.get(row).and_then(|row| row.get(column_index));
    let mut order: Vec<usize> = match filtered {
        Some(rows) => rows.to_vec(),
        None => (0..result.rows.len()).collect(),
    };
    // Stable, so rows that tie keep the order the server sent.
    order.sort_by(|left, right| {
        let ordering = match (value(*left), value(*right)) {
            (Some(left), Some(right)) => left.compare(right),
            (left, right) => left.is_some().cmp(&right.is_some()),
        };
        if descending {
            ordering.reverse()
        } else {
            ordering
        }
    });
    Some(Arc::new(order))
}

/// Rows with a value whose text contains `needle`, which must be lowercase.
pub fn matching_rows(result: &ResultSet, needle: &str) -> Vec<usize> {
    result
        .rows
        .iter()
        .enumerate()
        .filter(|(_, row)| {
            row.iter()
                .any(|value| value.to_string().to_lowercase().contains(needle))
        })
        .map(|(index, _)| index)
        .collect()
}

/// A value as it is copied: all of it, and nothing for NULL.
fn whole_value(result: &ResultSet, row: usize, column: usize) -> String {
    match result.rows.get(row).and_then(|row| row.get(column)) {
        None | Some(Value::Null) => String::new(),
        Some(value) => value.to_string(),
    }
}

/// Each column's name and the row's whole value, for the row viewer.
fn row_fields(result: &ResultSet, row: usize) -> Arc<Vec<(SharedString, SharedString)>> {
    let values = result.rows.get(row).map_or(&[][..], Vec::as_slice);
    Arc::new(
        result
            .columns
            .iter()
            .zip(values)
            .map(|(column, value)| {
                let text = value.to_string();
                let shown = match text.char_indices().nth(MAX_VIEWED_CHARACTERS) {
                    Some((end, _)) => format!("{}…", text.get(..end).unwrap_or(&text)),
                    None => text,
                };
                (
                    SharedString::from(column.name.clone()),
                    SharedString::from(shown),
                )
            })
            .collect(),
    )
}

/// Characters of a value a cell draws; shaping a megabyte of JSON per frame stalls scrolling.
const MAX_CELL_CHARACTERS: usize = 500;

/// Grid cells are one line high; newlines in text values would be clipped.
fn single_line(text: &str) -> SharedString {
    let shown = match text.char_indices().nth(MAX_CELL_CHARACTERS) {
        Some((end, _)) => text.get(..end).unwrap_or(text),
        None => text,
    };
    let mut line = if shown.contains(['\n', '\r']) {
        shown.replace("\r\n", "⏎").replace(['\n', '\r'], "⏎")
    } else {
        shown.to_owned()
    };
    if shown.len() < text.len() {
        line.push('…');
    }
    line.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_show_one_short_line() {
        assert_eq!(single_line("a\r\nb\nc"), "a⏎b⏎c");
        let long = "é".repeat(MAX_CELL_CHARACTERS + 10);
        let shown = single_line(&long);
        assert_eq!(shown.chars().count(), MAX_CELL_CHARACTERS + 1);
        assert!(shown.ends_with('…'));
        assert_eq!(single_line("short"), "short");
    }

    #[test]
    fn sorts_by_a_column_without_moving_rows() {
        let result = ResultSet::new(
            vec![sql_db::Column {
                name: "n".to_owned(),
                type_name: "int".to_owned(),
            }],
            vec![
                vec![Value::Int(3)],
                vec![Value::Null],
                vec![Value::Int(1)],
                vec![Value::Int(3)],
            ],
        );
        let order = |filtered: Option<Vec<usize>>, sort| {
            shown_order(&result, filtered.map(Arc::new).as_ref(), Some((0, sort)))
                .map(|order| order.to_vec())
        };
        assert_eq!(order(None, ColumnSort::Default), None);
        assert_eq!(order(None, ColumnSort::Ascending), Some(vec![1, 2, 0, 3]));
        assert_eq!(order(None, ColumnSort::Descending), Some(vec![0, 3, 2, 1]));
        // A filter narrows the rows before they are sorted.
        let threes = matching_rows(&result, "3");
        assert_eq!(threes, vec![0, 3]);
        assert_eq!(matching_rows(&result, "null"), vec![1]);
        assert_eq!(
            order(Some(vec![1, 2, 3]), ColumnSort::Descending),
            Some(vec![3, 2, 1])
        );
        assert_eq!(
            order(Some(threes.clone()), ColumnSort::Default),
            Some(threes)
        );
        assert_eq!(whole_value(&result, 1, 0), "");
        assert_eq!(whole_value(&result, 2, 0), "1");
        let fields = row_fields(&result, 2);
        assert_eq!(fields.as_slice(), [("n".into(), "1".into())]);
    }
}
