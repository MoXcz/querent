use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::{
    App, Context, IntoElement, ParentElement as _, SharedString, Styled as _, Window, div, px,
};
use std::sync::Arc;

use sql_db::{ResultSet, Value};

const CHARACTER_WIDTH: f32 = 7.5;
const MIN_COLUMN_WIDTH: f32 = 60.;
const MAX_COLUMN_WIDTH: f32 = 420.;
/// Rows sampled to size columns; scanning all rows would stall large results.
const WIDTH_SAMPLE_ROWS: usize = 200;

#[derive(Default)]
pub struct ResultsTable {
    result: Arc<ResultSet>,
    columns: Vec<Column>,
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
                .width(px(width));
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
        self.result.rows.len()
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
        let value = self
            .result
            .rows
            .get(row_index)
            .and_then(|row| row.get(column_index));
        match value {
            None | Some(Value::Null) => div().text_color(cx.theme().muted_foreground).child("NULL"),
            Some(value) => div().child(single_line(&value.to_string())),
        }
    }

    fn cell_text(&self, row_index: usize, column_index: usize, _: &App) -> String {
        self.result
            .rows
            .get(row_index)
            .and_then(|row| row.get(column_index))
            .map(ToString::to_string)
            .unwrap_or_default()
    }
}

/// Grid cells are one line high; newlines in text values would be clipped.
fn single_line(text: &str) -> SharedString {
    if text.contains(['\n', '\r']) {
        text.replace("\r\n", "⏎").replace(['\n', '\r'], "⏎").into()
    } else {
        SharedString::from(text.to_owned())
    }
}
