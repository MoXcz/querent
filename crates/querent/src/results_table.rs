use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, ClipboardItem, Context, Focusable as _, InteractiveElement as _, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled, Window, actions, div, prelude::FluentBuilder as _, px,
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

actions!(
    querent,
    [
        /// Copy the selected cells as tab-separated text, ready to paste into a spreadsheet.
        CopySelection,
        /// Copy the selected cells with a header line of their column names.
        CopySelectionWithHeaders
    ]
);

/// gpui-kit's padding around a cell at the grid's default size; a cell's own
/// element spreads over it so the selection fills the cell.
const CELL_PADDING_X: f32 = 8.;
const CELL_PADDING_Y: f32 = 4.;
/// The row-number column stays this narrow; long numbers are cut off rather than widen it.
const ROW_NUMBER_WIDTH: f32 = 32.;
const ROW_NUMBER_PADDING_X: f32 = 4.;
/// gpui-kit's header row height at the grid's default size, filled by the select-all corner.
const HEADER_HEIGHT: f32 = 32.;

/// A rectangle of grid cells, from the cell first pressed to the one the pointer is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellRange {
    /// `(grid row, column)`.
    anchor: (usize, usize),
    focus: (usize, usize),
}

impl CellRange {
    fn rows(&self) -> std::ops::RangeInclusive<usize> {
        self.anchor.0.min(self.focus.0)..=self.anchor.0.max(self.focus.0)
    }

    fn columns(&self) -> std::ops::Range<usize> {
        self.anchor.1.min(self.focus.1)..self.anchor.1.max(self.focus.1) + 1
    }

    fn contains(&self, row: usize, column: usize) -> bool {
        self.rows().contains(&row) && self.columns().contains(&column)
    }
}

/// The text a copy of the selected cells puts on the clipboard.
#[derive(Debug, PartialEq, Eq)]
pub enum CopiedRows {
    Nothing,
    TooMany(usize),
    Text(String),
}

#[derive(Default)]
pub struct ResultsTable {
    result: Arc<ResultSet>,
    columns: Vec<Column>,
    /// Rows the filter keeps, in the order the server sent them; `None` keeps every row.
    filtered: Option<Arc<Vec<usize>>>,
    sort: Option<(usize, ColumnSort)>,
    /// Row numbers of `result` as the grid shows them, after the filter and the sort; the rows are not copied.
    order: Option<Arc<Vec<usize>>>,
    /// Cells picked by pressing, dragging and Shift-clicking in the grid.
    cells: Option<CellRange>,
    /// The left button went down on a cell and has not been seen released.
    dragging: bool,
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
        if !self.columns.is_empty() {
            self.columns.insert(0, row_number_column());
        }
        self.result = result;
        self.filtered = None;
        self.sort = None;
        self.order = None;
        self.clear_selected();
    }

    /// Keeps only `rows` of the result, or every row for `None`.
    pub fn set_filtered(&mut self, rows: Option<Vec<usize>>) {
        self.filtered = rows.map(Arc::new);
        self.order = shown_order(&self.result, self.filtered.as_ref(), self.sort);
        self.clear_selected();
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

    /// The grid for a tab; the delegate keeps its own cell selection instead of the table's rows.
    pub fn new_grid(window: &mut Window, cx: &mut Context<TableState<Self>>) -> TableState<Self> {
        TableState::new(Self::default(), window, cx)
            .row_selectable(false)
            .col_selectable(false)
            // A moved column would leave `render_td`'s index and the copied columns out of step.
            .col_movable(false)
    }

    fn sort_by(&mut self, column_index: usize, sort: ColumnSort) {
        self.sort = Some((column_index, sort));
        self.order = shown_order(&self.result, self.filtered.as_ref(), self.sort);
        self.clear_selected();
    }

    /// A press on a row number selects that whole row, or with Shift every row from the anchor.
    fn press_row(&mut self, row: usize, extend: bool) {
        let Some(last_column) = self.result.columns.len().checked_sub(1) else {
            return;
        };
        let anchor_row = match self.cells {
            Some(range) if extend => range.anchor.0,
            Some(_) | None => row,
        };
        self.cells = Some(CellRange {
            anchor: (anchor_row, 0),
            focus: (row, last_column),
        });
        self.dragging = true;
    }

    /// Stretches a selection of whole rows to the row number under a dragging pointer.
    fn drag_rows_to(&mut self, row: usize, button_down: bool) -> bool {
        let last_column = self.result.columns.len().saturating_sub(1);
        self.drag_to(row, last_column, button_down)
    }

    /// Selects every cell of the result, as the corner above the row numbers does.
    fn select_all(&mut self) {
        let (Some(last_row), Some(last_column)) = (
            self.shown_count().checked_sub(1),
            self.result.columns.len().checked_sub(1),
        ) else {
            return;
        };
        self.cells = Some(CellRange {
            anchor: (0, 0),
            focus: (last_row, last_column),
        });
        self.dragging = false;
    }

    fn clear_selected(&mut self) {
        self.cells = None;
        self.dragging = false;
    }

    /// A left press on a cell starts a selection there, or with Shift stretches the current one.
    fn press_cell(&mut self, row: usize, column: usize, extend: bool) {
        self.cells = Some(match self.cells {
            Some(range) if extend => CellRange {
                focus: (row, column),
                ..range
            },
            Some(_) | None => CellRange {
                anchor: (row, column),
                focus: (row, column),
            },
        });
        self.dragging = true;
    }

    /// Stretches the selection to the cell under a dragging pointer; true when it changed.
    fn drag_to(&mut self, row: usize, column: usize, button_down: bool) -> bool {
        if !button_down {
            self.dragging = false;
        }
        let Some(range) = self.cells.as_mut().filter(|_| self.dragging) else {
            return false;
        };
        let changed = range.focus != (row, column);
        range.focus = (row, column);
        changed
    }

    /// The selected cells as tab-separated text, rows in the order the grid shows them.
    pub fn copy_selected(&self, headers: Headers) -> CopiedRows {
        let Some(range) = self.cells else {
            return CopiedRows::Nothing;
        };
        let count = range.rows().count();
        let total = self.result.rows.len().max(1);
        let estimated_bytes = self.result.estimated_bytes() / total * count;
        if count > MAX_COPIED_ROWS || estimated_bytes > MAX_COPIED_BYTES {
            return CopiedRows::TooMany(count);
        }
        let rows = range.rows().map(|row_index| self.source_row(row_index));
        CopiedRows::Text(sql_db::cells_as_tsv(
            &self.result,
            rows,
            range.columns(),
            headers,
        ))
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

    fn render_th(
        &mut self,
        column_index: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        if column_index != 0 {
            return div()
                .w_full()
                .child(self.column(column_index, cx).name)
                .into_any_element();
        }
        // The empty corner spreads over its padding so the whole square takes the click.
        div()
            .id("select-all")
            .mx(px(-ROW_NUMBER_PADDING_X))
            .my(px(-CELL_PADDING_Y))
            .h(px(HEADER_HEIGHT))
            .w(px(ROW_NUMBER_WIDTH))
            .hover(|corner| corner.bg(cx.theme().list_hover))
            .tooltip(|window, cx| {
                gpui_kit::component::tooltip::Tooltip::new("Select every cell; Ctrl+C copies them")
                    .build(window, cx)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|table, _: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    table.delegate_mut().select_all();
                    table.focus_handle(cx).focus(window, cx);
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    fn render_td(
        &mut self,
        row_index: usize,
        column_index: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(column_index) = data_column(column_index) else {
            let in_selection = self
                .cells
                .is_some_and(|range| range.rows().contains(&row_index));
            return div()
                .mx(px(-ROW_NUMBER_PADDING_X))
                .my(px(-CELL_PADDING_Y))
                .px(px(ROW_NUMBER_PADDING_X))
                .py(px(CELL_PADDING_Y))
                .text_xs()
                .text_color(if in_selection {
                    cx.theme().foreground
                } else {
                    cx.theme().muted_foreground
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |table, event: &MouseDownEvent, window, cx| {
                        let extend = event.modifiers.shift;
                        table.delegate_mut().press_row(row_index, extend);
                        table.focus_handle(cx).focus(window, cx);
                        cx.notify();
                    }),
                )
                .on_mouse_move(cx.listener(move |table, event: &MouseMoveEvent, _, cx| {
                    let button_down = event.pressed_button == Some(MouseButton::Left);
                    if table.delegate_mut().drag_rows_to(row_index, button_down) {
                        cx.notify();
                    }
                }))
                .child((row_index + 1).to_string())
                .into_any_element();
        };
        let content = match self.value(row_index, column_index) {
            None | Some(Value::Null) => div().text_color(cx.theme().muted_foreground).child("NULL"),
            Some(value) => div().child(single_line(&value.to_string())),
        };
        let selected = self
            .cells
            .filter(|range| range.contains(row_index, column_index));
        // The editor's selection color, so a selection looks the same in the grid and the text.
        let fill = cx.theme().selection;
        let border = fill.alpha(1.);
        div()
            .mx(px(-CELL_PADDING_X))
            .my(px(-CELL_PADDING_Y))
            .px(px(CELL_PADDING_X))
            .py(px(CELL_PADDING_Y))
            .when_some(selected, |cell, range| {
                // The outline runs only along the rectangle's outer edges.
                let rows = range.rows();
                let columns = range.columns();
                cell.bg(fill)
                    .border_color(border)
                    .when(row_index == *rows.start(), Styled::border_t_1)
                    .when(row_index == *rows.end(), Styled::border_b_1)
                    .when(column_index == columns.start, Styled::border_l_1)
                    .when(column_index + 1 == columns.end, Styled::border_r_1)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |table, event: &MouseDownEvent, window, cx| {
                    let extend = event.modifiers.shift;
                    table
                        .delegate_mut()
                        .press_cell(row_index, column_index, extend);
                    table.focus_handle(cx).focus(window, cx);
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(move |table, event: &MouseMoveEvent, _, cx| {
                let button_down = event.pressed_button == Some(MouseButton::Left);
                if table
                    .delegate_mut()
                    .drag_to(row_index, column_index, button_down)
                {
                    cx.notify();
                }
            }))
            .child(content)
            .into_any_element()
    }

    fn cell_text(&self, row_index: usize, column_index: usize, _: &App) -> String {
        let Some(column_index) = data_column(column_index) else {
            return (row_index + 1).to_string();
        };
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
        if let Some(column_index) = data_column(column_index) {
            self.sort_by(column_index, sort);
        }
        cx.notify();
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
            .when(self.cells.is_some(), |menu| {
                menu.menu("Copy selection", Box::new(CopySelection))
                    .menu(
                        "Copy selection with headers",
                        Box::new(CopySelectionWithHeaders),
                    )
                    .separator()
            })
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

/// The result column a grid column shows; `None` for the row numbers, which come first.
fn data_column(table_column: usize) -> Option<usize> {
    table_column.checked_sub(1)
}

/// The narrow, pinned column of row numbers in grid order; not data, so never sorted or copied.
fn row_number_column() -> Column {
    Column::new("row-number", "")
        .width(px(ROW_NUMBER_WIDTH))
        .paddings(gpui_kit::Edges {
            top: px(CELL_PADDING_Y),
            bottom: px(CELL_PADDING_Y),
            left: px(ROW_NUMBER_PADDING_X),
            right: px(ROW_NUMBER_PADDING_X),
        })
        .text_right()
        .fixed_left()
        .resizable(false)
        .movable(false)
        .selectable(false)
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
    fn presses_and_drags_select_a_rectangle() {
        let mut table = ResultsTable::default();
        assert!(!table.drag_to(3, 3, true));
        table.press_cell(4, 2, false);
        assert!(table.drag_to(1, 0, true));
        let range = table.cells.unwrap();
        assert_eq!(range.rows(), 1..=4);
        assert_eq!(range.columns(), 0..3);
        assert!(range.contains(2, 1) && !range.contains(2, 3));
        // A move with the button up ends the drag.
        assert!(!table.drag_to(9, 9, false));
        assert_eq!(table.cells.unwrap().focus, (1, 0));
        table.press_cell(6, 1, true);
        assert_eq!(table.cells.unwrap().anchor, (4, 2));
        table.press_cell(6, 1, false);
        assert_eq!(table.cells.unwrap().rows(), 6..=6);
    }

    #[test]
    fn the_corner_selects_every_cell_and_row_numbers_lead() {
        let mut table = ResultsTable::default();
        table.select_all();
        assert_eq!(table.cells, None);
        table.set_result(Arc::new(ResultSet::new(
            vec![
                sql_db::Column {
                    name: "a".to_owned(),
                    type_name: "int".to_owned(),
                },
                sql_db::Column {
                    name: "b".to_owned(),
                    type_name: "int".to_owned(),
                },
            ],
            vec![
                vec![Value::Int(1), Value::Int(2)],
                vec![Value::Int(3), Value::Int(4)],
            ],
        )));
        assert_eq!(table.columns.len(), 3);
        table.select_all();
        assert_eq!(
            table.copy_selected(Headers::Include),
            CopiedRows::Text("a\tb\n1\t2\n3\t4\n".to_owned())
        );
        table.press_row(1, false);
        assert_eq!(
            table.copy_selected(Headers::Omit),
            CopiedRows::Text("3\t4\n".to_owned())
        );
        table.press_row(0, true);
        assert_eq!(table.cells.unwrap().rows(), 0..=1);
        assert_eq!(table.cells.unwrap().columns(), 0..2);
        table.set_result(Arc::new(ResultSet::default()));
        assert!(table.columns.is_empty());
    }

    #[test]
    fn copies_selected_cells_in_grid_order() {
        let column = |name: &str| sql_db::Column {
            name: name.to_owned(),
            type_name: "int".to_owned(),
        };
        let mut table = ResultsTable::default();
        table.set_result(Arc::new(ResultSet::new(
            vec![column("id"), column("name"), column("note")],
            vec![
                vec![Value::Int(1), Value::Text("a".to_owned()), Value::Null],
                vec![Value::Int(2), Value::Null, Value::Null],
                vec![Value::Int(3), Value::Text("c\td".to_owned()), Value::Null],
            ],
        )));
        assert_eq!(table.copy_selected(Headers::Include), CopiedRows::Nothing);
        table.press_cell(0, 0, false);
        table.sort_by(0, ColumnSort::Descending);
        // Sorting clears the selection: its grid rows now show other rows.
        assert_eq!(table.copy_selected(Headers::Omit), CopiedRows::Nothing);
        table.press_cell(0, 0, false);
        table.drag_to(1, 1, true);
        assert_eq!(
            table.copy_selected(Headers::Include),
            CopiedRows::Text("id\tname\n3\t\"c\td\"\n2\t\n".to_owned())
        );
        assert_eq!(
            table.copy_selected(Headers::Omit),
            CopiedRows::Text("3\t\"c\td\"\n2\t\n".to_owned())
        );
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
