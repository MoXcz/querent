use std::io::Write;

use crate::{ResultSet, Value};

/// Writes a header row of column names, then one row per result row.
/// NULL becomes an empty field so spreadsheets read it as blank.
pub fn write_csv(result_set: &ResultSet, writer: impl Write) -> csv::Result<()> {
    let mut csv_writer = csv::Writer::from_writer(writer);
    csv_writer.write_record(result_set.columns.iter().map(|column| column.name.as_str()))?;
    for row in &result_set.rows {
        csv_writer.write_record(row.iter().map(csv_field))?;
    }
    csv_writer.flush()?;
    Ok(())
}

fn csv_field(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Decimal(_)
        | Value::Text(_)
        | Value::Bytes(_)
        | Value::Date(_)
        | Value::Time(_)
        | Value::Timestamp(_) => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Column;

    fn column(name: &str) -> Column {
        Column {
            name: name.to_owned(),
            type_name: "NVarChar".to_owned(),
        }
    }

    #[test]
    fn quotes_special_characters_and_round_trips() {
        let result_set = ResultSet {
            columns: vec![column("name"), column("note, with comma"), column("amount")],
            rows: vec![
                vec![
                    Value::Text("say \"hi\"".to_owned()),
                    Value::Text("line one\nline two".to_owned()),
                    Value::Decimal("12.50".to_owned()),
                ],
                vec![Value::Text("a,b".to_owned()), Value::Null, Value::Int(-3)],
            ],
            truncated: false,
            ..ResultSet::default()
        };
        let mut bytes = Vec::new();
        write_csv(&result_set, &mut bytes).unwrap();

        let mut reader = csv::Reader::from_reader(bytes.as_slice());
        assert_eq!(
            reader.headers().unwrap(),
            vec!["name", "note, with comma", "amount"]
        );
        let rows: Vec<csv::StringRecord> = reader.records().map(Result::unwrap).collect();
        assert_eq!(rows[0], vec!["say \"hi\"", "line one\nline two", "12.50"]);
        assert_eq!(rows[1], vec!["a,b", "", "-3"]);
    }

    #[test]
    fn writes_bytes_bools_and_dates_as_displayed() {
        let result_set = ResultSet {
            columns: vec![column("b"), column("flag"), column("day")],
            rows: vec![vec![
                Value::Bytes(vec![0x0a, 0xff]),
                Value::Bool(true),
                Value::Date("2024-02-29".to_owned()),
            ]],
            truncated: false,
            ..ResultSet::default()
        };
        let mut bytes = Vec::new();
        write_csv(&result_set, &mut bytes).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "b,flag,day\n0x0AFF,1,2024-02-29\n"
        );
    }

    #[test]
    fn empty_result_writes_only_the_header() {
        let result_set = ResultSet {
            columns: vec![column("id")],
            ..ResultSet::default()
        };
        let mut bytes = Vec::new();
        write_csv(&result_set, &mut bytes).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "id\n");
    }
}
