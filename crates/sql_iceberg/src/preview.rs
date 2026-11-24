//! A table's first rows, read from its data files with its delete files applied.

use arrow_array::cast::AsArray as _;
use arrow_array::types::{
    Float16Type, Float32Type, Float64Type, Int8Type, Int16Type, Int32Type, Int64Type, UInt8Type,
    UInt16Type, UInt32Type, UInt64Type,
};
use arrow_array::{Array, RecordBatch};
use arrow_cast::display::{ArrayFormatter, FormatOptions};
use arrow_schema::DataType;
use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use sql_db::{Column, ResultSet, Value};

use crate::{IcebergError, Result, TableInfo};

/// Which rows a preview reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewScope {
    /// The table as of the current snapshot.
    Current,
    /// The table as of an earlier snapshot.
    Snapshot(i64),
    /// One data file of a snapshot, with the deletes that apply to it.
    File { snapshot: i64, path: String },
}

impl TableInfo {
    /// Up to `limit` rows; the result is marked truncated when more were left.
    pub async fn preview(&self, scope: &PreviewScope, limit: usize) -> Result<ResultSet> {
        let mut scan = self.table.scan().select_all();
        match scope {
            PreviewScope::Current => {}
            PreviewScope::Snapshot(snapshot) | PreviewScope::File { snapshot, .. } => {
                scan = scan.snapshot_id(*snapshot);
            }
        }
        let scan = scan.build()?;
        let mut tasks: Vec<_> = scan.plan_files().await?.try_collect().await?;
        if let PreviewScope::File { path, .. } = scope {
            tasks.retain(|task| task.data_file_path() == path);
            if tasks.is_empty() {
                return Err(IcebergError::NotADataFile(path.clone()));
            }
        }
        let batches = self
            .table
            .reader_builder()
            .build()
            .read(stream::iter(tasks.into_iter().map(Ok)).boxed())?
            .stream();
        let mut batches = std::pin::pin!(batches);
        let mut columns = None;
        let mut rows = Vec::new();
        let mut truncated = false;
        // Breaking out at the limit drops the stream, so files not yet started are never read.
        while let Some(batch) = batches.next().await {
            let batch = batch?;
            if columns.is_none() {
                columns = Some(self.columns_of(&batch));
            }
            let room = limit.saturating_sub(rows.len());
            if batch.num_rows() > room {
                truncated = true;
            }
            rows.extend(batch_rows(&batch, room));
            if truncated {
                break;
            }
        }
        let mut result = ResultSet::new(columns.unwrap_or_else(|| self.empty_columns()), rows);
        result.truncated = truncated;
        Ok(result)
    }

    /// Columns named as in the batch, typed with Iceberg's names where the schema has them.
    fn columns_of(&self, batch: &RecordBatch) -> Vec<Column> {
        let schema = self.table.metadata().current_schema();
        batch
            .schema()
            .fields()
            .iter()
            .map(|field| Column {
                name: field.name().clone(),
                type_name: schema.as_struct().field_by_name(field.name()).map_or_else(
                    || field.data_type().to_string(),
                    |known| known.field_type.to_string(),
                ),
            })
            .collect()
    }

    /// The schema's top-level columns, for a table or file with no rows.
    fn empty_columns(&self) -> Vec<Column> {
        self.table
            .metadata()
            .current_schema()
            .as_struct()
            .fields()
            .iter()
            .map(|field| Column {
                name: field.name.clone(),
                type_name: field.field_type.to_string(),
            })
            .collect()
    }
}

/// The first `limit` rows of a batch as grid values.
fn batch_rows(batch: &RecordBatch, limit: usize) -> Vec<Vec<Value>> {
    let count = batch.num_rows().min(limit);
    let options = FormatOptions::default();
    let columns: Vec<Vec<Value>> = batch
        .columns()
        .iter()
        .map(|array| column_values(array.as_ref(), count, &options))
        .collect();
    (0..count)
        .map(|row| {
            columns
                .iter()
                .map(|values| values.get(row).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .collect()
}

fn column_values(array: &dyn Array, count: usize, options: &FormatOptions<'_>) -> Vec<Value> {
    // Each row through `value`, with nulls kept as nulls.
    let each = |value: &dyn Fn(usize) -> Value| -> Vec<Value> {
        (0..count)
            .map(|row| {
                if array.is_null(row) {
                    Value::Null
                } else {
                    value(row)
                }
            })
            .collect()
    };
    macro_rules! integers {
        ($type:ty) => {{
            let values = array.as_primitive::<$type>();
            each(&|row| Value::Int(i64::from(values.value(row))))
        }};
    }
    // Through the shortest text, so 0.1 as f32 stays 0.1 rather than 0.10000000149011612.
    macro_rules! narrow_floats {
        ($type:ty) => {{
            let values = array.as_primitive::<$type>();
            each(&|row| {
                let value = values.value(row);
                Value::Float(value.to_string().parse().unwrap_or(f64::from(value)))
            })
        }};
    }
    match array.data_type() {
        DataType::Int8 => integers!(Int8Type),
        DataType::Int16 => integers!(Int16Type),
        DataType::Int32 => integers!(Int32Type),
        DataType::Int64 => integers!(Int64Type),
        DataType::UInt8 => integers!(UInt8Type),
        DataType::UInt16 => integers!(UInt16Type),
        DataType::UInt32 => integers!(UInt32Type),
        DataType::UInt64 => {
            let values = array.as_primitive::<UInt64Type>();
            each(&|row| {
                let value = values.value(row);
                i64::try_from(value).map_or_else(|_| Value::Text(value.to_string()), Value::Int)
            })
        }
        DataType::Float16 => {
            let values = array.as_primitive::<Float16Type>();
            each(&|row| {
                let value = values.value(row);
                Value::Float(value.to_string().parse().unwrap_or(value.to_f64()))
            })
        }
        DataType::Float32 => narrow_floats!(Float32Type),
        DataType::Float64 => {
            let values = array.as_primitive::<Float64Type>();
            each(&|row| Value::Float(values.value(row)))
        }
        DataType::Boolean => {
            let values = array.as_boolean();
            each(&|row| Value::Bool(values.value(row)))
        }
        DataType::Binary
        | DataType::LargeBinary
        | DataType::FixedSizeBinary(_)
        | DataType::BinaryView => each(&|row| Value::Bytes(binary_at(array, row))),
        DataType::Date32 | DataType::Date64 => formatted(array, count, options, Value::Date),
        DataType::Time32(_) | DataType::Time64(_) => formatted(array, count, options, Value::Time),
        DataType::Timestamp(..) => timestamps(array, count, options),
        DataType::Decimal32(..)
        | DataType::Decimal64(..)
        | DataType::Decimal128(..)
        | DataType::Decimal256(..) => formatted(array, count, options, Value::Decimal),
        DataType::Null
        | DataType::Duration(_)
        | DataType::Interval(_)
        | DataType::Utf8
        | DataType::LargeUtf8
        | DataType::Utf8View
        | DataType::List(_)
        | DataType::ListView(_)
        | DataType::FixedSizeList(..)
        | DataType::LargeList(_)
        | DataType::LargeListView(_)
        | DataType::Struct(_)
        | DataType::Union(..)
        | DataType::Dictionary(..)
        | DataType::Map(..)
        | DataType::RunEndEncoded(..) => formatted(array, count, options, Value::Text),
    }
}

fn binary_at(array: &dyn Array, row: usize) -> Vec<u8> {
    if let Some(values) = array.as_binary_opt::<i32>() {
        values.value(row).to_vec()
    } else if let Some(values) = array.as_binary_opt::<i64>() {
        values.value(row).to_vec()
    } else if let Some(values) = array.as_fixed_size_binary_opt() {
        values.value(row).to_vec()
    } else if let Some(values) = array.as_binary_view_opt() {
        values.value(row).to_vec()
    } else {
        Vec::new()
    }
}

fn timestamps(array: &dyn Array, count: usize, options: &FormatOptions<'_>) -> Vec<Value> {
    if let DataType::Timestamp(unit, Some(zone)) = array.data_type()
        && ArrayFormatter::try_new(array, options).is_err()
    {
        // Named zones need arrow's tz database; Iceberg's are UTC, so the zoneless time is the same.
        if let Ok(utc) = arrow_cast::cast(array, &DataType::Timestamp(*unit, None)) {
            return formatted(utc.as_ref(), count, options, |text| {
                Value::Timestamp(format!("{text} {zone}"))
            });
        }
    }
    formatted(array, count, options, Value::Timestamp)
}

/// Values as arrow prints them, wrapped by `wrap`; nulls stay null.
fn formatted(
    array: &dyn Array,
    count: usize,
    options: &FormatOptions<'_>,
    wrap: impl Fn(String) -> Value,
) -> Vec<Value> {
    let Ok(formatter) = ArrayFormatter::try_new(array, options) else {
        return vec![Value::Text(format!("({})", array.data_type())); count];
    };
    (0..count)
        .map(|row| {
            if array.is_null(row) {
                Value::Null
            } else {
                wrap(formatter.value(row).to_string())
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use arrow_array::{BinaryArray, BooleanArray, Date32Array, Int32Array, StringArray};
    use arrow_schema::{Field, Schema};

    fn batch() -> RecordBatch {
        let schema = Schema::new(vec![
            Field::new("id", DataType::Int32, true),
            Field::new("station", DataType::Utf8, true),
            Field::new("day", DataType::Date32, true),
            Field::new("late", DataType::Boolean, true),
            Field::new("raw", DataType::Binary, true),
        ]);
        RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(Int32Array::from(vec![Some(1), None, Some(3)])),
                Arc::new(StringArray::from(vec![Some("Delft"), Some("Leiden"), None])),
                Arc::new(Date32Array::from(vec![Some(20_727), None, Some(0)])),
                Arc::new(BooleanArray::from(vec![Some(true), Some(false), None])),
                Arc::new(BinaryArray::from(vec![
                    Some(&[0x0a_u8, 0xff][..]),
                    None,
                    Some(&[][..]),
                ])),
            ],
        )
        .unwrap()
    }

    #[test]
    fn batches_become_grid_rows_with_nulls_kept() {
        let rows = batch_rows(&batch(), 10);
        assert_eq!(
            rows.first(),
            Some(&vec![
                Value::Int(1),
                Value::Text("Delft".to_owned()),
                Value::Date("2026-10-01".to_owned()),
                Value::Bool(true),
                Value::Bytes(vec![0x0a, 0xff]),
            ])
        );
        assert_eq!(
            rows.get(1),
            Some(&vec![
                Value::Null,
                Value::Text("Leiden".to_owned()),
                Value::Null,
                Value::Bool(false),
                Value::Null,
            ])
        );
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn narrow_floats_and_zoned_timestamps_read_as_written() {
        use arrow_array::{Float32Array, TimestampMicrosecondArray};
        let floats = Float32Array::from(vec![Some(0.1_f32)]);
        assert_eq!(
            column_values(&floats, 1, &FormatOptions::default()),
            vec![Value::Float(0.1)]
        );
        for zone in ["+00:00", "UTC"] {
            let stamps = TimestampMicrosecondArray::from(vec![Some(0)]).with_timezone(zone);
            let values = column_values(&stamps, 1, &FormatOptions::default());
            let Some(Value::Timestamp(text)) = values.first() else {
                panic!("{zone}: {values:?}");
            };
            assert!(text.starts_with("1970-01-01T00:00:00"), "{zone}: {text}");
        }
    }

    #[test]
    fn a_limit_cuts_the_batch() {
        assert_eq!(batch_rows(&batch(), 2).len(), 2);
        assert_eq!(batch_rows(&batch(), 0).len(), 0);
    }
}
