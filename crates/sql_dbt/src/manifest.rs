//! The parts of dbt's `manifest.json` and `catalog.json` the editor reads; other fields are ignored.

use std::collections::{BTreeMap, HashMap};

use serde::Deserialize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceType {
    Model,
    Source,
    Seed,
    Snapshot,
    Test,
    Analysis,
    Macro,
    Exposure,
    /// Kinds this version does not show, such as metrics and unit tests.
    #[default]
    #[serde(other)]
    Other,
}

impl ResourceType {
    /// The kinds the tree lists, in its order.
    pub const SHOWN: [Self; 8] = [
        Self::Model,
        Self::Source,
        Self::Seed,
        Self::Snapshot,
        Self::Test,
        Self::Analysis,
        Self::Macro,
        Self::Exposure,
    ];

    #[must_use]
    pub fn plural_label(self) -> &'static str {
        match self {
            Self::Model => "Models",
            Self::Source => "Sources",
            Self::Seed => "Seeds",
            Self::Snapshot => "Snapshots",
            Self::Test => "Tests",
            Self::Analysis => "Analyses",
            Self::Macro => "Macros",
            Self::Exposure => "Exposures",
            Self::Other => "Other",
        }
    }

    #[must_use]
    pub fn singular_label(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Source => "source",
            Self::Seed => "seed",
            Self::Snapshot => "snapshot",
            Self::Test => "test",
            Self::Analysis => "analysis",
            Self::Macro => "macro",
            Self::Exposure => "exposure",
            Self::Other => "other",
        }
    }

    /// Whether the node is a table or view in the warehouse that can be queried.
    #[must_use]
    pub fn is_relation(self) -> bool {
        match self {
            Self::Model | Self::Source | Self::Seed | Self::Snapshot => true,
            Self::Test | Self::Analysis | Self::Macro | Self::Exposure | Self::Other => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDoc {
    pub name: String,
    pub description: String,
    /// From the YAML, or from `catalog.json` when the warehouse was read.
    pub data_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub unique_id: String,
    pub resource_type: ResourceType,
    pub name: String,
    /// The `source()` group a source table belongs to.
    pub source_name: Option<String>,
    pub package_name: String,
    /// Relative to the project root, with `/` between parts.
    pub original_file_path: String,
    pub materialized: Option<String>,
    pub tags: Vec<String>,
    pub group: Option<String>,
    pub description: String,
    pub columns: Vec<ColumnDoc>,
    /// `[database].[schema].[name]` as the adapter quotes it.
    pub relation_name: Option<String>,
    pub database: Option<String>,
    pub schema: Option<String>,
    /// The name of the built table or view: the alias, or a source's identifier.
    pub relation: Option<String>,
    pub raw_code: String,
}

impl Node {
    /// `source.table` for sources, the name for everything else.
    #[must_use]
    pub fn display_name(&self) -> String {
        match &self.source_name {
            Some(source) => format!("{source}.{}", self.name),
            None => self.name.clone(),
        }
    }

    #[must_use]
    pub fn is_view(&self) -> bool {
        self.materialized.as_deref() == Some("view")
    }
}

#[derive(Debug, Default)]
pub struct Manifest {
    pub project_name: String,
    pub adapter_type: String,
    pub nodes: BTreeMap<String, Node>,
    pub parents: HashMap<String, Vec<String>>,
    pub children: HashMap<String, Vec<String>>,
}

impl Manifest {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        let raw: RawManifest = serde_json::from_str(json)?;
        let nodes = [raw.nodes, raw.sources, raw.exposures, raw.macros]
            .into_iter()
            .flatten()
            .map(|(unique_id, node)| {
                let node = node.into_node(unique_id.clone());
                (unique_id, node)
            })
            .collect();
        Ok(Self {
            project_name: raw.metadata.project_name,
            adapter_type: raw.metadata.adapter_type,
            nodes,
            parents: raw.parent_map,
            children: raw.child_map,
        })
    }

    /// The model, seed or snapshot `ref('name')` points at, preferring `package`.
    #[must_use]
    pub fn find_ref(&self, package: Option<&str>, name: &str) -> Option<&Node> {
        let mut matches = self.nodes.values().filter(|node| {
            node.name == name
                && matches!(
                    node.resource_type,
                    ResourceType::Model | ResourceType::Seed | ResourceType::Snapshot
                )
                && package.is_none_or(|package| node.package_name == package)
        });
        let first = matches.next()?;
        Some(
            std::iter::once(first)
                .chain(matches)
                .find(|node| node.package_name == self.project_name)
                .unwrap_or(first),
        )
    }

    #[must_use]
    pub fn find_source(&self, source_name: &str, table: &str) -> Option<&Node> {
        self.nodes.values().find(|node| {
            node.resource_type == ResourceType::Source
                && node.source_name.as_deref() == Some(source_name)
                && node.name == table
        })
    }

    /// Copies the warehouse's column types into the nodes it describes.
    pub fn apply_catalog(&mut self, catalog: &Catalog) {
        for (unique_id, columns) in &catalog.columns {
            let Some(node) = self.nodes.get_mut(unique_id) else {
                continue;
            };
            for column in columns {
                match node
                    .columns
                    .iter_mut()
                    .find(|known| known.name.eq_ignore_ascii_case(&column.name))
                {
                    Some(known) => known.data_type = Some(column.data_type.clone()),
                    None => node.columns.push(ColumnDoc {
                        name: column.name.clone(),
                        description: String::new(),
                        data_type: Some(column.data_type.clone()),
                    }),
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogColumn {
    pub name: String,
    pub data_type: String,
}

/// Column types `dbt docs generate` read from the warehouse.
#[derive(Debug, Default)]
pub struct Catalog {
    pub columns: HashMap<String, Vec<CatalogColumn>>,
}

impl Catalog {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        let raw: RawCatalog = serde_json::from_str(json)?;
        let columns = raw
            .nodes
            .into_iter()
            .chain(raw.sources)
            .map(|(unique_id, entry)| {
                let mut columns: Vec<RawCatalogColumn> = entry.columns.into_values().collect();
                columns.sort_by_key(|column| column.index);
                let columns = columns
                    .into_iter()
                    .map(|column| CatalogColumn {
                        name: column.name,
                        data_type: column.data_type,
                    })
                    .collect();
                (unique_id, columns)
            })
            .collect();
        Ok(Self { columns })
    }
}

#[derive(Deserialize)]
struct RawManifest {
    #[serde(default)]
    metadata: RawMetadata,
    #[serde(default)]
    nodes: HashMap<String, RawNode>,
    #[serde(default)]
    sources: HashMap<String, RawNode>,
    #[serde(default)]
    exposures: HashMap<String, RawNode>,
    #[serde(default)]
    macros: HashMap<String, RawNode>,
    #[serde(default)]
    parent_map: HashMap<String, Vec<String>>,
    #[serde(default)]
    child_map: HashMap<String, Vec<String>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawMetadata {
    project_name: String,
    adapter_type: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawNode {
    resource_type: ResourceType,
    name: String,
    source_name: Option<String>,
    package_name: String,
    original_file_path: String,
    config: RawConfig,
    tags: Vec<String>,
    group: Option<String>,
    description: String,
    columns: BTreeMap<String, RawColumn>,
    relation_name: Option<String>,
    database: Option<String>,
    schema: Option<String>,
    alias: Option<String>,
    identifier: Option<String>,
    raw_code: String,
    /// Older manifests call `raw_code` this.
    raw_sql: String,
}

impl RawNode {
    fn into_node(self, unique_id: String) -> Node {
        let relation = self
            .identifier
            .or(self.alias)
            .or_else(|| self.relation_name.is_some().then(|| self.name.clone()));
        Node {
            unique_id,
            resource_type: self.resource_type,
            name: self.name,
            source_name: self.source_name,
            package_name: self.package_name,
            original_file_path: self.original_file_path.replace('\\', "/"),
            materialized: self.config.materialized,
            tags: self.tags,
            group: self.group,
            description: self.description,
            columns: self
                .columns
                .into_values()
                .map(|column| ColumnDoc {
                    name: column.name,
                    description: column.description,
                    data_type: column.data_type,
                })
                .collect(),
            relation_name: self.relation_name,
            database: self.database,
            schema: self.schema,
            relation,
            raw_code: if self.raw_code.is_empty() {
                self.raw_sql
            } else {
                self.raw_code
            },
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawConfig {
    materialized: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawColumn {
    name: String,
    description: String,
    data_type: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawCatalog {
    nodes: HashMap<String, RawCatalogEntry>,
    sources: HashMap<String, RawCatalogEntry>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawCatalogEntry {
    columns: HashMap<String, RawCatalogColumn>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawCatalogColumn {
    name: String,
    #[serde(rename = "type")]
    data_type: String,
    index: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"{
        "metadata": {"project_name": "cima", "adapter_type": "fabric", "dbt_version": "9.9"},
        "nodes": {
            "model.cima.stg_orders": {
                "resource_type": "model", "name": "stg_orders", "package_name": "cima",
                "original_file_path": "models/staging/stg_orders.sql",
                "config": {"materialized": "view", "unknown": 1},
                "tags": ["ventas"], "columns": {"id": {"name": "id", "description": "Key"}},
                "relation_name": "[analytics].[staging].[stg_orders]",
                "database": "analytics", "schema": "staging", "alias": "stg_orders",
                "raw_code": "select * from {{ source('erp', 'orders') }}",
                "future_field": {"nested": true}
            },
            "test.cima.not_null": {"resource_type": "test", "name": "not_null", "package_name": "cima"},
            "metric.cima.x": {"resource_type": "metric", "name": "x"}
        },
        "sources": {
            "source.cima.erp.orders": {
                "resource_type": "source", "name": "orders", "source_name": "erp",
                "package_name": "cima", "relation_name": "[raw].[erp].[orders]",
                "identifier": "ORDERS"
            }
        },
        "parent_map": {"model.cima.stg_orders": ["source.cima.erp.orders"]},
        "child_map": {"source.cima.erp.orders": ["model.cima.stg_orders"]}
    }"#;

    #[test]
    fn reads_the_fields_it_needs_and_ignores_the_rest() {
        let manifest = Manifest::parse(MANIFEST).unwrap();
        assert_eq!(manifest.project_name, "cima");
        let model = &manifest.nodes["model.cima.stg_orders"];
        assert_eq!(model.resource_type, ResourceType::Model);
        assert!(model.is_view());
        assert_eq!(model.columns[0].description, "Key");
        assert_eq!(
            manifest.nodes["metric.cima.x"].resource_type,
            ResourceType::Other
        );
        let source = manifest.find_source("erp", "orders").unwrap();
        assert_eq!(source.display_name(), "erp.orders");
        assert_eq!(source.relation.as_deref(), Some("ORDERS"));
        assert_eq!(
            manifest.find_ref(None, "stg_orders").unwrap().name,
            "stg_orders"
        );
        assert!(manifest.find_ref(None, "not_null").is_none());
        assert_eq!(
            manifest.parents["model.cima.stg_orders"],
            ["source.cima.erp.orders"]
        );
    }

    #[test]
    fn catalog_types_fill_in_columns() {
        let mut manifest = Manifest::parse(MANIFEST).unwrap();
        let catalog = Catalog::parse(
            r#"{"nodes": {"model.cima.stg_orders": {"columns": {
                "total": {"name": "total", "type": "decimal", "index": 2},
                "ID": {"name": "ID", "type": "int", "index": 1}
            }}}}"#,
        )
        .unwrap();
        manifest.apply_catalog(&catalog);
        let columns = &manifest.nodes["model.cima.stg_orders"].columns;
        assert_eq!(columns[0].data_type.as_deref(), Some("int"));
        assert_eq!(columns[1].name, "total");
    }
}
