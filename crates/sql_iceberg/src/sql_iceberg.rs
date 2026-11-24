//! Iceberg REST catalogs, read-only: namespaces, tables, and each table's metadata
//! down to its manifests and files. Needs a tokio runtime; no UI.

mod preview;

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use iceberg::io::LocalFsStorageFactory;
use iceberg::spec::{
    DataContentType, DataFileFormat, Datum, Literal, ManifestContentType, ManifestFile,
    ManifestStatus, PrimitiveLiteral, PrimitiveType, Struct, Transform, Type,
};
use iceberg::table::Table;
use iceberg::{Catalog as _, CatalogBuilder as _, NamespaceIdent, TableIdent};
use iceberg_catalog_rest::{
    REST_CATALOG_PROP_URI, REST_CATALOG_PROP_WAREHOUSE, RestCatalog, RestCatalogBuilder,
};
use sql_db::{Column, ResultSet, Value};

pub use preview::PreviewScope;

#[derive(Debug, thiserror::Error)]
pub enum IcebergError {
    #[error(transparent)]
    Iceberg(#[from] iceberg::Error),
    #[error("table has no snapshot {0}")]
    NoSnapshot(i64),
    #[error("{0} is not a data file of that snapshot")]
    NotADataFile(String),
    #[error("snapshot {snapshot} has no manifest {path}")]
    NoManifest { snapshot: i64, path: String },
}

pub type Result<T> = std::result::Result<T, IcebergError>;

/// How to reach a REST catalog.
#[derive(Clone, PartialEq, Eq)]
pub struct CatalogConfig {
    pub uri: String,
    /// The warehouse the catalog serves, when it serves more than one.
    pub warehouse: Option<String>,
    /// A bearer token, or `client_id:client_secret` for the catalog's OAuth endpoint.
    pub credential: Option<String>,
}

impl fmt::Debug for CatalogConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CatalogConfig")
            .field("uri", &self.uri)
            .field("warehouse", &self.warehouse)
            .field("credential", &self.credential.as_ref().map(|_| "<set>"))
            .finish()
    }
}

impl CatalogConfig {
    fn properties(&self) -> HashMap<String, String> {
        let mut properties = HashMap::from([(REST_CATALOG_PROP_URI.to_owned(), self.uri.clone())]);
        if let Some(warehouse) = self.warehouse.as_ref().filter(|value| !value.is_empty()) {
            properties.insert(REST_CATALOG_PROP_WAREHOUSE.to_owned(), warehouse.clone());
        }
        if let Some(credential) = self.credential.as_ref().filter(|value| !value.is_empty()) {
            // A token is opaque; client credentials are always `id:secret`.
            let key = if credential.contains(':') {
                "credential"
            } else {
                "token"
            };
            properties.insert(key.to_owned(), credential.clone());
        }
        properties
    }
}

pub struct Catalog {
    inner: RestCatalog,
}

impl fmt::Debug for Catalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Catalog").finish_non_exhaustive()
    }
}

impl Catalog {
    pub async fn connect(config: &CatalogConfig) -> Result<Self> {
        let inner = RestCatalogBuilder::default()
            // Data and metadata files are read from local disk; object stores come later.
            .with_storage_factory(Arc::new(LocalFsStorageFactory))
            .load("rest", config.properties())
            .await?;
        // The builder does not call the server; listing does, so a wrong URI fails here.
        inner.list_namespaces(None).await?;
        Ok(Self { inner })
    }

    /// Namespaces under `parent`, or the top-level ones, each as its full path.
    pub async fn namespaces(&self, parent: &[String]) -> Result<Vec<Vec<String>>> {
        let parent = if parent.is_empty() {
            None
        } else {
            Some(NamespaceIdent::from_vec(parent.to_vec())?)
        };
        let mut namespaces: Vec<Vec<String>> = self
            .inner
            .list_namespaces(parent.as_ref())
            .await?
            .into_iter()
            .map(NamespaceIdent::inner)
            .collect();
        namespaces.sort();
        Ok(namespaces)
    }

    pub async fn tables(&self, namespace: &[String]) -> Result<Vec<String>> {
        let namespace = NamespaceIdent::from_vec(namespace.to_vec())?;
        let mut tables: Vec<String> = self
            .inner
            .list_tables(&namespace)
            .await?
            .into_iter()
            .map(|table| table.name)
            .collect();
        tables.sort();
        Ok(tables)
    }

    pub async fn table(&self, namespace: &[String], name: &str) -> Result<TableInfo> {
        let ident = TableIdent::new(
            NamespaceIdent::from_vec(namespace.to_vec())?,
            name.to_owned(),
        );
        let table = self.inner.load_table(&ident).await?;
        Ok(TableInfo::read(table))
    }
}

/// A column of the table's current schema; nested fields have dotted names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub id: i32,
    pub name: String,
    pub type_name: String,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionField {
    pub name: String,
    /// Such as `identity(service_date)` or `bucket[16](id)`.
    pub transform: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub id: i64,
    pub parent: Option<i64>,
    pub sequence_number: i64,
    pub timestamp_ms: i64,
    /// `append`, `overwrite`, `delete` or `replace`.
    pub operation: String,
    /// Counts such as `added-records`, sorted by key.
    pub summary: Vec<(String, String)>,
    pub manifest_list: String,
}

impl SnapshotInfo {
    #[must_use]
    pub fn summary_value(&self, key: &str) -> Option<&str> {
        self.summary
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    /// Rows added and removed, such as `+9000 -500`, for a one-line label.
    #[must_use]
    pub fn change(&self) -> String {
        let mut parts = Vec::new();
        if let Some(added) = self.summary_value("added-records") {
            parts.push(format!("+{added}"));
        }
        if let Some(deleted) = self.summary_value("deleted-records") {
            parts.push(format!("-{deleted}"));
        }
        if let Some(deletes) = self.summary_value("added-position-deletes") {
            parts.push(format!("{deletes} position deletes"));
        }
        if let Some(deletes) = self.summary_value("added-equality-deletes") {
            parts.push(format!("{deletes} equality deletes"));
        }
        parts.join(" ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataLogEntry {
    pub file: String,
    pub timestamp_ms: i64,
}

/// A loaded table: its metadata as plain values, and the handle that reads its files.
pub struct TableInfo {
    table: Table,
    pub location: String,
    pub metadata_location: Option<String>,
    pub format_version: String,
    pub current_snapshot: Option<i64>,
    pub fields: Vec<Field>,
    pub partition: Vec<PartitionField>,
    pub properties: Vec<(String, String)>,
    /// Oldest first.
    pub snapshots: Vec<SnapshotInfo>,
    /// Earlier metadata files, oldest first; the current one is `metadata_location`.
    pub metadata_log: Vec<MetadataLogEntry>,
}

impl fmt::Debug for TableInfo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TableInfo")
            .field("location", &self.location)
            .field("snapshots", &self.snapshots.len())
            .finish_non_exhaustive()
    }
}

impl TableInfo {
    fn read(table: Table) -> Self {
        let metadata = table.metadata();
        let schema = metadata.current_schema();
        let mut fields = Vec::new();
        for field in schema.as_struct().fields() {
            push_field(
                &mut fields,
                "",
                field.id,
                &field.name,
                &field.field_type,
                field.required,
            );
        }
        let partition = metadata
            .default_partition_spec()
            .fields()
            .iter()
            .map(|field| {
                let source = schema
                    .name_by_field_id(field.source_id)
                    .map_or_else(|| field.source_id.to_string(), str::to_owned);
                PartitionField {
                    name: field.name.clone(),
                    transform: format!("{}({source})", field.transform),
                }
            })
            .collect();
        let mut properties: Vec<(String, String)> = metadata
            .properties()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        properties.sort();
        let mut snapshots: Vec<SnapshotInfo> = metadata
            .snapshots()
            .map(|snapshot| {
                let summary = snapshot.summary();
                let mut values: Vec<(String, String)> = summary
                    .additional_properties
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                values.sort();
                SnapshotInfo {
                    id: snapshot.snapshot_id(),
                    parent: snapshot.parent_snapshot_id(),
                    sequence_number: snapshot.sequence_number(),
                    timestamp_ms: snapshot.timestamp_ms(),
                    operation: summary.operation.as_str().to_owned(),
                    summary: values,
                    manifest_list: snapshot.manifest_list().to_owned(),
                }
            })
            .collect();
        snapshots.sort_by_key(|snapshot| (snapshot.sequence_number, snapshot.timestamp_ms));
        let metadata_log = metadata
            .metadata_log()
            .iter()
            .map(|entry| MetadataLogEntry {
                file: entry.metadata_file.clone(),
                timestamp_ms: entry.timestamp_ms,
            })
            .collect();
        Self {
            location: metadata.location().to_owned(),
            metadata_location: table.metadata_location().map(str::to_owned),
            format_version: metadata.format_version().to_string(),
            current_snapshot: metadata.current_snapshot_id(),
            fields,
            partition,
            properties,
            snapshots,
            metadata_log,
            table,
        }
    }

    /// The manifests a snapshot's manifest list names.
    pub async fn manifests(&self, snapshot_id: i64) -> Result<Vec<ManifestInfo>> {
        let snapshot = self
            .table
            .metadata()
            .snapshot_by_id(snapshot_id)
            .ok_or(IcebergError::NoSnapshot(snapshot_id))?;
        let list = self.table.manifest_list_reader(snapshot).load().await?;
        Ok(list.entries().iter().map(ManifestInfo::new).collect())
    }

    /// The data and delete files one manifest tracks, removed ones included.
    pub async fn files(&self, manifest: &ManifestInfo) -> Result<Vec<FileInfo>> {
        let read = self.table.manifest_reader().read(&manifest.file).await?;
        let metadata = self.table.metadata();
        let spec = metadata.partition_spec_by_id(manifest.file.partition_spec_id);
        let partition_type = spec
            .and_then(|spec| spec.partition_type(metadata.current_schema()).ok())
            .map(|partition| {
                partition
                    .fields()
                    .iter()
                    .map(|field| (*field.field_type).clone())
                    .collect::<Vec<Type>>()
            })
            .unwrap_or_default();
        let transforms: Vec<(String, Transform)> = spec
            .map(|spec| {
                spec.fields()
                    .iter()
                    .map(|field| (field.name.clone(), field.transform))
                    .collect()
            })
            .unwrap_or_default();
        Ok(read
            .entries()
            .iter()
            .map(|entry| {
                let file = entry.data_file();
                FileInfo {
                    status: status_label(entry.status()),
                    content: content_label(file.content_type()),
                    path: file.file_path().to_owned(),
                    format: format_label(file.file_format()),
                    partition: partition_label(file.partition(), &transforms, &partition_type),
                    record_count: file.record_count(),
                    size_bytes: file.file_size_in_bytes(),
                }
            })
            .collect())
    }

    /// Every file a snapshot reads, through all of its manifests.
    pub async fn snapshot_files(&self, snapshot_id: i64) -> Result<Vec<FileInfo>> {
        let mut files = Vec::new();
        for manifest in self.manifests(snapshot_id).await? {
            files.extend(self.files(&manifest).await?);
        }
        Ok(files)
    }

    /// The manifest at `path` in a snapshot's list.
    pub async fn manifest(&self, snapshot_id: i64, path: &str) -> Result<ManifestInfo> {
        self.manifests(snapshot_id)
            .await?
            .into_iter()
            .find(|manifest| manifest.path == path)
            .ok_or_else(|| IcebergError::NoManifest {
                snapshot: snapshot_id,
                path: path.to_owned(),
            })
    }
}

fn push_field(
    fields: &mut Vec<Field>,
    prefix: &str,
    id: i32,
    name: &str,
    field_type: &Type,
    required: bool,
) {
    let name = format!("{prefix}{name}");
    fields.push(Field {
        id,
        name: name.clone(),
        type_name: field_type.to_string(),
        required,
    });
    if let Type::Struct(nested) = field_type {
        for field in nested.fields() {
            push_field(
                fields,
                &format!("{name}."),
                field.id,
                &field.name,
                &field.field_type,
                field.required,
            );
        }
    }
}

#[derive(Debug, Clone)]
pub struct ManifestInfo {
    file: ManifestFile,
    pub path: String,
    /// `data` or `deletes`.
    pub content: &'static str,
    pub added_snapshot: i64,
    pub sequence_number: i64,
    pub added_files: Option<u32>,
    pub existing_files: Option<u32>,
    pub deleted_files: Option<u32>,
    pub added_rows: Option<u64>,
    pub existing_rows: Option<u64>,
    pub deleted_rows: Option<u64>,
}

impl ManifestInfo {
    fn new(file: &ManifestFile) -> Self {
        Self {
            path: file.manifest_path.clone(),
            content: match file.content {
                ManifestContentType::Data => "data",
                ManifestContentType::Deletes => "deletes",
            },
            added_snapshot: file.added_snapshot_id,
            sequence_number: file.sequence_number,
            added_files: file.added_files_count,
            existing_files: file.existing_files_count,
            deleted_files: file.deleted_files_count,
            added_rows: file.added_rows_count,
            existing_rows: file.existing_rows_count,
            deleted_rows: file.deleted_rows_count,
            file: file.clone(),
        }
    }

    /// Files the manifest tracks as live: added plus existing.
    #[must_use]
    pub fn live_files(&self) -> u32 {
        self.added_files.unwrap_or(0) + self.existing_files.unwrap_or(0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    /// `added`, `existing` or `deleted` in this manifest.
    pub status: &'static str,
    /// `data`, `position deletes` or `equality deletes`.
    pub content: &'static str,
    pub path: String,
    pub format: &'static str,
    /// Such as `service_date=2026-10-01`; empty for an unpartitioned table.
    pub partition: String,
    pub record_count: u64,
    pub size_bytes: u64,
}

fn status_label(status: ManifestStatus) -> &'static str {
    match status {
        ManifestStatus::Added => "added",
        ManifestStatus::Existing => "existing",
        ManifestStatus::Deleted => "deleted",
    }
}

fn content_label(content: DataContentType) -> &'static str {
    match content {
        DataContentType::Data => "data",
        DataContentType::PositionDeletes => "position deletes",
        DataContentType::EqualityDeletes => "equality deletes",
    }
}

fn format_label(format: DataFileFormat) -> &'static str {
    match format {
        DataFileFormat::Avro => "avro",
        DataFileFormat::Orc => "orc",
        DataFileFormat::Parquet => "parquet",
        DataFileFormat::Puffin => "puffin",
    }
}

fn partition_label(values: &Struct, transforms: &[(String, Transform)], types: &[Type]) -> String {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let name = transforms
                .get(index)
                .map_or_else(|| index.to_string(), |(name, _)| name.clone());
            let transform = transforms.get(index).map(|(_, transform)| *transform);
            let text = value.map_or_else(
                || "null".to_owned(),
                |value| literal_text(value, transform, types.get(index)),
            );
            format!("{name}={text}")
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// A partition value as Iceberg's own tools print it, such as `2026-10` for a month.
fn literal_text(
    value: &Literal,
    transform: Option<Transform>,
    value_type: Option<&Type>,
) -> String {
    let Some(primitive) = value.as_primitive_literal() else {
        return format!("{value:?}");
    };
    let primitive_type = value_type.and_then(Type::as_primitive_type);
    match (&primitive, transform, primitive_type) {
        (PrimitiveLiteral::Int(years), Some(Transform::Year), _) => (1970 + years).to_string(),
        (PrimitiveLiteral::Int(months), Some(Transform::Month), _) => {
            let year = 1970 + months.div_euclid(12);
            let month = months.rem_euclid(12) + 1;
            format!("{year}-{month:02}")
        }
        (PrimitiveLiteral::Int(hours), Some(Transform::Hour), _) => {
            let micros = i64::from(*hours) * 3_600_000_000;
            let text = Datum::timestamp_micros(micros).to_string();
            // `2026-10-01 10:00:00` becomes `2026-10-01-10`.
            text.get(..13)
                .map_or_else(|| text.clone(), |hour| hour.replacen(' ', "-", 1))
        }
        (PrimitiveLiteral::Int(days), _, Some(PrimitiveType::Date)) => {
            Datum::date(*days).to_string()
        }
        (PrimitiveLiteral::Long(micros), _, Some(PrimitiveType::Timestamp)) => {
            Datum::timestamp_micros(*micros).to_string()
        }
        (PrimitiveLiteral::Long(micros), _, Some(PrimitiveType::Timestamptz)) => {
            Datum::timestamptz_micros(*micros).to_string()
        }
        _ => primitive_text(&primitive),
    }
}

fn primitive_text(value: &PrimitiveLiteral) -> String {
    match value {
        PrimitiveLiteral::Boolean(value) => value.to_string(),
        PrimitiveLiteral::Int(value) => value.to_string(),
        PrimitiveLiteral::Long(value) => value.to_string(),
        PrimitiveLiteral::Float(value) => value.to_string(),
        PrimitiveLiteral::Double(value) => value.to_string(),
        PrimitiveLiteral::String(value) => value.clone(),
        PrimitiveLiteral::Binary(bytes) => bytes
            .iter()
            .flat_map(|byte| [byte >> 4, byte & 0xf])
            .filter_map(|digit| char::from_digit(u32::from(digit), 16))
            .collect(),
        PrimitiveLiteral::Int128(value) => value.to_string(),
        PrimitiveLiteral::UInt128(value) => value.to_string(),
        PrimitiveLiteral::AboveMax => "above max".to_owned(),
        PrimitiveLiteral::BelowMin => "below min".to_owned(),
    }
}

/// A byte count for a label, such as `12.4 KB`.
#[must_use]
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut unit = 0;
    let mut scale: u128 = 1;
    while u128::from(bytes) >= scale * 1024 && unit + 1 < UNITS.len() {
        scale *= 1024;
        unit += 1;
    }
    let unit_name = UNITS.get(unit).copied().unwrap_or("B");
    if unit == 0 {
        return format!("{bytes} {unit_name}");
    }
    let tenths = (u128::from(bytes) * 10 + scale / 2) / scale;
    format!("{}.{} {unit_name}", tenths / 10, tenths % 10)
}

/// The last path segment, which is what tells files apart in a tree.
#[must_use]
pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn columns(names: &[(&str, &str)]) -> Vec<Column> {
    names
        .iter()
        .map(|(name, type_name)| Column {
            name: (*name).to_owned(),
            type_name: (*type_name).to_owned(),
        })
        .collect()
}

fn optional_int(value: Option<impl Into<i64>>) -> Value {
    value.map_or(Value::Null, |value| Value::Int(value.into()))
}

fn count(value: u64) -> Value {
    i64::try_from(value).map_or_else(|_| Value::Text(value.to_string()), Value::Int)
}

fn optional_count(value: Option<u64>) -> Value {
    value.map_or(Value::Null, count)
}

/// Milliseconds since the epoch as a UTC timestamp, to the second.
#[must_use]
pub fn timestamp_text(timestamp_ms: i64) -> String {
    let seconds = timestamp_ms.div_euclid(1000);
    let (days, second_of_day) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    let (year, month, day) = civil_date(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        second_of_day / 3600,
        second_of_day % 3600 / 60,
        second_of_day % 60
    )
}

/// Year, month and day of a count of days since 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn civil_date(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[must_use]
pub fn fields_result(fields: &[Field]) -> ResultSet {
    ResultSet::new(
        columns(&[
            ("id", "int"),
            ("name", "string"),
            ("type", "string"),
            ("required", "boolean"),
        ]),
        fields
            .iter()
            .map(|field| {
                vec![
                    Value::Int(field.id.into()),
                    Value::Text(field.name.clone()),
                    Value::Text(field.type_name.clone()),
                    Value::Bool(field.required),
                ]
            })
            .collect(),
    )
}

#[must_use]
pub fn snapshots_result(snapshots: &[SnapshotInfo], current: Option<i64>) -> ResultSet {
    let mut keys: Vec<&str> = snapshots
        .iter()
        .flat_map(|snapshot| snapshot.summary.iter().map(|(key, _)| key.as_str()))
        .collect();
    keys.sort_unstable();
    keys.dedup();
    let mut names = vec![
        ("snapshot_id", "long"),
        ("parent_id", "long"),
        ("sequence_number", "long"),
        ("committed_at", "timestamptz"),
        ("operation", "string"),
        ("is_current", "boolean"),
    ];
    names.extend(keys.iter().map(|key| (*key, "string")));
    names.push(("manifest_list", "string"));
    let rows = snapshots
        .iter()
        .rev()
        .map(|snapshot| {
            let mut row = vec![
                Value::Int(snapshot.id),
                optional_int(snapshot.parent),
                Value::Int(snapshot.sequence_number),
                Value::Timestamp(timestamp_text(snapshot.timestamp_ms)),
                Value::Text(snapshot.operation.clone()),
                Value::Bool(current == Some(snapshot.id)),
            ];
            row.extend(keys.iter().map(|key| {
                snapshot
                    .summary_value(key)
                    .map_or(Value::Null, |value| Value::Text(value.to_owned()))
            }));
            row.push(Value::Text(snapshot.manifest_list.clone()));
            row
        })
        .collect();
    ResultSet::new(columns(&names), rows)
}

#[must_use]
pub fn manifests_result(manifests: &[ManifestInfo]) -> ResultSet {
    ResultSet::new(
        columns(&[
            ("content", "string"),
            ("added_snapshot_id", "long"),
            ("sequence_number", "long"),
            ("added_files", "int"),
            ("existing_files", "int"),
            ("deleted_files", "int"),
            ("added_rows", "long"),
            ("existing_rows", "long"),
            ("deleted_rows", "long"),
            ("path", "string"),
        ]),
        manifests
            .iter()
            .map(|manifest| {
                vec![
                    Value::Text(manifest.content.to_owned()),
                    Value::Int(manifest.added_snapshot),
                    Value::Int(manifest.sequence_number),
                    optional_int(manifest.added_files),
                    optional_int(manifest.existing_files),
                    optional_int(manifest.deleted_files),
                    optional_count(manifest.added_rows),
                    optional_count(manifest.existing_rows),
                    optional_count(manifest.deleted_rows),
                    Value::Text(manifest.path.clone()),
                ]
            })
            .collect(),
    )
}

#[must_use]
pub fn files_result(files: &[FileInfo]) -> ResultSet {
    ResultSet::new(
        columns(&[
            ("status", "string"),
            ("content", "string"),
            ("partition", "string"),
            ("record_count", "long"),
            ("file_size_in_bytes", "long"),
            ("format", "string"),
            ("path", "string"),
        ]),
        files
            .iter()
            .map(|file| {
                vec![
                    Value::Text(file.status.to_owned()),
                    Value::Text(file.content.to_owned()),
                    Value::Text(file.partition.clone()),
                    count(file.record_count),
                    count(file.size_bytes),
                    Value::Text(file.format.to_owned()),
                    Value::Text(file.path.clone()),
                ]
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_and_client_credentials_go_to_different_properties() {
        let mut config = CatalogConfig {
            uri: "http://localhost:8181".to_owned(),
            warehouse: Some(String::new()),
            credential: Some("eyJhbGciOi".to_owned()),
        };
        let properties = config.properties();
        assert_eq!(
            properties.get("token").map(String::as_str),
            Some("eyJhbGciOi")
        );
        assert!(!properties.contains_key(REST_CATALOG_PROP_WAREHOUSE));
        config.credential = Some("client:secret".to_owned());
        let properties = config.properties();
        assert_eq!(
            properties.get("credential").map(String::as_str),
            Some("client:secret")
        );
        assert!(!properties.contains_key("token"));
    }

    #[test]
    fn debug_output_hides_the_credential() {
        let config = CatalogConfig {
            uri: "http://localhost:8181".to_owned(),
            warehouse: None,
            credential: Some("secret-token".to_owned()),
        };
        assert!(!format!("{config:?}").contains("secret-token"));
    }

    #[test]
    fn partition_values_print_like_iceberg() {
        let date = Type::Primitive(PrimitiveType::Date);
        let int = Type::Primitive(PrimitiveType::Int);
        let october_first = Literal::Primitive(PrimitiveLiteral::Int(20_727));
        assert_eq!(
            literal_text(&october_first, Some(Transform::Identity), Some(&date)),
            "2026-10-01"
        );
        assert_eq!(
            literal_text(&october_first, Some(Transform::Day), Some(&date)),
            "2026-10-01"
        );
        let month = Literal::Primitive(PrimitiveLiteral::Int(56 * 12 + 9));
        assert_eq!(
            literal_text(&month, Some(Transform::Month), Some(&int)),
            "2026-10"
        );
        let year = Literal::Primitive(PrimitiveLiteral::Int(56));
        assert_eq!(
            literal_text(&year, Some(Transform::Year), Some(&int)),
            "2026"
        );
        let hour = Literal::Primitive(PrimitiveLiteral::Int(20_727 * 24 + 10));
        assert_eq!(
            literal_text(&hour, Some(Transform::Hour), Some(&int)),
            "2026-10-01-10"
        );
        let bucket = Literal::Primitive(PrimitiveLiteral::Int(7));
        assert_eq!(
            literal_text(&bucket, Some(Transform::Bucket(16)), Some(&int)),
            "7"
        );
        let bytes = Literal::Primitive(PrimitiveLiteral::Binary(vec![0x0a, 0xff]));
        assert_eq!(
            literal_text(&bytes, Some(Transform::Identity), None),
            "0aff"
        );
        let station = Literal::Primitive(PrimitiveLiteral::String("Delft".to_owned()));
        assert_eq!(
            literal_text(&station, Some(Transform::Identity), None),
            "Delft"
        );
    }

    #[test]
    fn partition_label_names_each_field() {
        let values = Struct::from_iter([
            Some(Literal::Primitive(PrimitiveLiteral::Int(20_727))),
            None,
        ]);
        let transforms = vec![
            ("service_date".to_owned(), Transform::Identity),
            ("station".to_owned(), Transform::Identity),
        ];
        let types = vec![
            Type::Primitive(PrimitiveType::Date),
            Type::Primitive(PrimitiveType::String),
        ];
        assert_eq!(
            partition_label(&values, &transforms, &types),
            "service_date=2026-10-01/station=null"
        );
        assert_eq!(partition_label(&Struct::empty(), &[], &[]), "");
    }

    #[test]
    fn sizes_read_in_the_largest_whole_unit() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(12_700), "12.4 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(human_size(u64::MAX), "16777216.0 TB");
    }

    #[test]
    fn a_snapshot_change_lists_what_it_added_and_removed() {
        let snapshot = SnapshotInfo {
            id: 1,
            parent: None,
            sequence_number: 1,
            timestamp_ms: 0,
            operation: "delete".to_owned(),
            summary: vec![
                ("added-position-deletes".to_owned(), "333".to_owned()),
                ("total-records".to_owned(), "12000".to_owned()),
            ],
            manifest_list: String::new(),
        };
        assert_eq!(snapshot.change(), "333 position deletes");
    }

    #[test]
    fn snapshot_rows_put_the_newest_first_with_every_summary_key() {
        let older = SnapshotInfo {
            id: 1,
            parent: None,
            sequence_number: 1,
            timestamp_ms: 0,
            operation: "append".to_owned(),
            summary: vec![("added-records".to_owned(), "9000".to_owned())],
            manifest_list: "snap-1.avro".to_owned(),
        };
        let newer = SnapshotInfo {
            id: 2,
            parent: Some(1),
            sequence_number: 2,
            timestamp_ms: 1000,
            operation: "delete".to_owned(),
            summary: vec![("deleted-records".to_owned(), "5".to_owned())],
            manifest_list: "snap-2.avro".to_owned(),
        };
        let result = snapshots_result(&[older, newer], Some(2));
        let names: Vec<&str> = result
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "snapshot_id",
                "parent_id",
                "sequence_number",
                "committed_at",
                "operation",
                "is_current",
                "added-records",
                "deleted-records",
                "manifest_list"
            ]
        );
        assert_eq!(
            result.rows.first().and_then(|row| row.first()),
            Some(&Value::Int(2))
        );
        assert_eq!(
            result.rows.first().and_then(|row| row.get(5)),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            result.rows.get(1).and_then(|row| row.get(7)),
            Some(&Value::Null)
        );
    }

    #[test]
    fn timestamps_read_as_utc() {
        assert_eq!(timestamp_text(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(timestamp_text(1_500), "1970-01-01 00:00:01 UTC");
        assert_eq!(timestamp_text(1_795_782_000_123), "2026-11-27 12:20:00 UTC");
        assert_eq!(timestamp_text(951_782_400_000), "2000-02-29 00:00:00 UTC");
        assert_eq!(timestamp_text(-1_000), "1969-12-31 23:59:59 UTC");
    }
}
