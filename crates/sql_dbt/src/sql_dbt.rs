//! A dbt project read from what dbt writes in `target/`; needs no dbt install and changes nothing.

mod jinja;
mod manifest;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub use jinja::{NameContext, RenderError, name_context, render_refs, swap_database};
pub use manifest::{Catalog, CatalogColumn, ColumnDoc, Manifest, Node, ResourceType};

const PROJECT_FILE: &str = "dbt_project.yml";
/// Folders that never hold the user's own project: installed packages, build output, tooling.
const SKIPPED_FOLDERS: [&str; 6] = [
    "dbt_packages",
    "target",
    "logs",
    "node_modules",
    "venv",
    "__pycache__",
];

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("no target/manifest.json yet; run `dbt parse` or `dbt compile` in the project")]
    NoManifest,
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not a manifest this version can read: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug)]
pub struct Project {
    pub root: PathBuf,
    /// The `name:` in `dbt_project.yml`, which is also the package of its own nodes.
    pub name: String,
    pub target_dir: PathBuf,
    pub manifest: Manifest,
    pub manifest_modified: Option<SystemTime>,
    pub has_catalog: bool,
    /// Nodes whose file changed after the manifest was written.
    pub stale: HashSet<String>,
}

/// Where the editor finds a node's SQL for a preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlOrigin {
    /// `target/compiled/...`, written by `dbt compile` or `dbt run`.
    Compiled,
    /// `ref()` and `source()` filled in from the manifest.
    Rendered,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewSql {
    pub sql: String,
    pub origin: SqlOrigin,
    /// The model changed after the manifest was written, so the SQL may be old.
    pub stale: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Upstream,
    Downstream,
}

/// The folder dbt writes its artifacts to: `target-path`, or `target`. Blocking: reads a file.
#[must_use]
pub fn target_dir(root: &Path) -> PathBuf {
    let settings = std::fs::read_to_string(root.join(PROJECT_FILE)).unwrap_or_default();
    let (_, target_path) = project_settings(&settings);
    root.join(target_path.unwrap_or_else(|| "target".to_owned()))
}

/// Loads a project: blocking file reads, so call it off the UI thread.
pub fn load(root: &Path) -> Result<Project, LoadError> {
    let read = |path: PathBuf| {
        std::fs::read_to_string(&path).map_err(|source| LoadError::Read { path, source })
    };
    let settings = read(root.join(PROJECT_FILE))?;
    let (name, _) = project_settings(&settings);
    let target_dir = target_dir(root);
    let manifest_path = target_dir.join("manifest.json");
    let manifest_text = match std::fs::read_to_string(&manifest_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(LoadError::NoManifest);
        }
        Err(source) => {
            return Err(LoadError::Read {
                path: manifest_path,
                source,
            });
        }
    };
    let mut manifest = Manifest::parse(&manifest_text).map_err(|source| LoadError::Parse {
        path: manifest_path.clone(),
        source,
    })?;
    let catalog_path = target_dir.join("catalog.json");
    // An unreadable catalog only costs the column types.
    let catalog = std::fs::read_to_string(&catalog_path)
        .ok()
        .and_then(|text| Catalog::parse(&text).ok());
    if let Some(catalog) = &catalog {
        manifest.apply_catalog(catalog);
    }
    let manifest_modified = modified(&manifest_path);
    let name = name.unwrap_or_else(|| manifest.project_name.clone());
    let stale = stale_nodes(root, &manifest, &name, manifest_modified);
    Ok(Project {
        root: root.to_path_buf(),
        name,
        target_dir,
        manifest,
        manifest_modified,
        has_catalog: catalog.is_some(),
        stale,
    })
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn stale_nodes(
    root: &Path,
    manifest: &Manifest,
    project: &str,
    manifest_modified: Option<SystemTime>,
) -> HashSet<String> {
    let Some(written) = manifest_modified else {
        return HashSet::new();
    };
    manifest
        .nodes
        .values()
        .filter(|node| node.package_name == project && !node.original_file_path.is_empty())
        .filter(|node| {
            modified(&root.join(&node.original_file_path)).is_some_and(|changed| changed > written)
        })
        .map(|node| node.unique_id.clone())
        .collect()
}

/// `name:` and `target-path:` from `dbt_project.yml`, read as top-level `key: value` lines.
fn project_settings(yaml: &str) -> (Option<String>, Option<String>) {
    let value = |key: &str| {
        yaml.lines().find_map(|line| {
            let rest = line.strip_prefix(key)?.strip_prefix(':')?;
            let rest = rest.split(" #").next().unwrap_or(rest).trim();
            let unquoted = rest.trim_matches(['"', '\'']);
            (!unquoted.is_empty()).then(|| unquoted.to_owned())
        })
    };
    (value("name"), value("target-path"))
}

/// Folders under `roots` that hold a `dbt_project.yml`, up to `max_depth` levels down.
/// Blocking: walks the disk.
#[must_use]
pub fn find_projects(roots: &[PathBuf], max_depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in roots {
        find_in(root, max_depth, &mut found);
    }
    found.sort();
    found.dedup();
    found
}

fn find_in(directory: &Path, depth_left: usize, found: &mut Vec<PathBuf>) {
    if directory.join(PROJECT_FILE).is_file() {
        found.push(directory.to_path_buf());
        return;
    }
    if depth_left == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            !name.starts_with('.') && !SKIPPED_FOLDERS.contains(&name.as_ref())
        })
        .map(|entry| entry.path())
        .collect();
    children.sort();
    for child in children {
        find_in(&child, depth_left - 1, found);
    }
}

impl Project {
    #[must_use]
    pub fn file_of(&self, node: &Node) -> PathBuf {
        self.root.join(&node.original_file_path)
    }

    /// Whether the node is a `.sql` file dbt compiles; generic tests live in YAML instead.
    #[must_use]
    pub fn has_sql_file(node: &Node) -> bool {
        Path::new(&node.original_file_path)
            .extension()
            .is_some_and(|extension| extension == "sql")
    }

    #[must_use]
    pub fn compiled_file_of(&self, node: &Node) -> PathBuf {
        self.target_dir
            .join("compiled")
            .join(&node.package_name)
            .join(&node.original_file_path)
    }

    /// The node's SQL with every `ref()` resolved: the compiled file, or the
    /// raw code rendered from the manifest. Blocking: reads the compiled file.
    pub fn preview_sql(&self, node: &Node) -> Result<PreviewSql, RenderError> {
        let stale = self.stale.contains(&node.unique_id);
        if let Ok(sql) = std::fs::read_to_string(self.compiled_file_of(node)) {
            return Ok(PreviewSql {
                sql: sql.trim().to_owned(),
                origin: SqlOrigin::Compiled,
                stale,
            });
        }
        render_refs(&node.raw_code, &self.manifest).map(|sql| PreviewSql {
            sql,
            origin: SqlOrigin::Rendered,
            stale,
        })
    }

    /// The nodes this one reads from, or the ones that read from it, by name.
    #[must_use]
    pub fn lineage(&self, unique_id: &str, direction: Direction) -> Vec<&Node> {
        let map = match direction {
            Direction::Upstream => &self.manifest.parents,
            Direction::Downstream => &self.manifest.children,
        };
        let mut nodes: Vec<&Node> = map
            .get(unique_id)
            .into_iter()
            .flatten()
            .filter_map(|id| self.manifest.nodes.get(id))
            .filter(|node| node.resource_type != ResourceType::Test)
            .collect();
        nodes.sort_by_key(|node| node.display_name());
        nodes
    }

    /// Every node reachable upstream or downstream, each with its distance in steps
    /// (1 for a direct parent or child), nearest first.
    #[must_use]
    pub fn lineage_all(&self, unique_id: &str, direction: Direction) -> Vec<(&Node, usize)> {
        let mut distances: HashMap<&str, usize> = HashMap::from([(unique_id, 0)]);
        let mut found = Vec::new();
        let mut frontier = vec![unique_id];
        let mut distance = 0;
        while !frontier.is_empty() {
            distance += 1;
            let mut next = Vec::new();
            for id in frontier {
                for node in self.lineage(id, direction) {
                    let node_id = node.unique_id.as_str();
                    if distances.contains_key(node_id) {
                        continue;
                    }
                    distances.insert(node_id, distance);
                    found.push((node, distance));
                    next.push(node_id);
                }
            }
            frontier = next;
        }
        found.sort_by_cached_key(|(node, distance)| (*distance, node.display_name()));
        found
    }

    /// The project's own nodes of `kind`; installed packages are left out.
    pub fn nodes_of(&self, kind: ResourceType) -> impl Iterator<Item = &Node> {
        self.manifest
            .nodes
            .values()
            .filter(move |node| node.resource_type == kind && node.package_name == self.name)
    }
}

/// The folder a node is listed under: its file's folder below `models/`, `seeds/`
/// and so on, or the source group for sources.
#[must_use]
pub fn folder_of(node: &Node) -> String {
    if let Some(source) = &node.source_name {
        return source.clone();
    }
    let mut parts: Vec<&str> = node.original_file_path.split('/').collect();
    parts.pop();
    parts.get(1..).unwrap_or_default().join("/")
}

/// Whether `node` fits the filter, which must be lowercase: `tag:x`, `group:x`, or part of a name.
#[must_use]
pub fn matches_filter(node: &Node, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    if let Some(tag) = filter.strip_prefix("tag:") {
        return node
            .tags
            .iter()
            .any(|candidate| candidate.to_lowercase().starts_with(tag));
    }
    if let Some(group) = filter.strip_prefix("group:") {
        return node
            .group
            .as_deref()
            .is_some_and(|candidate| candidate.to_lowercase().starts_with(group));
    }
    node.display_name().to_lowercase().contains(filter)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempProject(PathBuf);

    impl Drop for TempProject {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn temp(name: &str) -> TempProject {
        let path = std::env::temp_dir().join(format!("sql-dbt-{name}-{}", std::process::id()));
        if path.exists() {
            std::fs::remove_dir_all(&path).unwrap();
        }
        std::fs::create_dir_all(&path).unwrap();
        TempProject(path)
    }

    #[test]
    fn reads_name_and_target_path() {
        let yaml =
            "name: 'cima'\nprofile: x\ntarget-path: \"build\" # moved\nmodels:\n  name: inner\n";
        assert_eq!(
            project_settings(yaml),
            (Some("cima".to_owned()), Some("build".to_owned()))
        );
        assert_eq!(project_settings("models: {}"), (None, None));
    }

    #[test]
    fn finds_projects_but_not_their_packages() {
        let folder = temp("find");
        let root = &folder.0;
        write(
            &root.join("dags/dbt/analytics/dbt_project.yml"),
            "name: a\n",
        );
        write(
            &root.join("dags/dbt/analytics/dbt_packages/utils/dbt_project.yml"),
            "name: u\n",
        );
        write(&root.join("a/b/c/d/e/dbt_project.yml"), "name: deep\n");
        write(&root.join(".venv/x/dbt_project.yml"), "name: hidden\n");
        let found = find_projects(std::slice::from_ref(root), 4);
        assert_eq!(found, [root.join("dags/dbt/analytics")]);
    }

    #[test]
    fn loads_a_project_and_prefers_compiled_sql() {
        let folder = temp("load");
        let root = &folder.0;
        write(&root.join("dbt_project.yml"), "name: cima\n");
        assert!(matches!(load(root), Err(LoadError::NoManifest)));
        write(
            &root.join("target/manifest.json"),
            r#"{"metadata": {"project_name": "cima"}, "nodes": {
                "model.cima.a": {"resource_type": "model", "name": "a", "package_name": "cima",
                    "original_file_path": "models/marts/a.sql", "raw_code": "select 1 as x",
                    "relation_name": "[db].[s].[a]", "tags": ["Ventas"]},
                "model.cima.b": {"resource_type": "model", "name": "b", "package_name": "cima",
                    "original_file_path": "models/b.sql", "raw_code": "select * from {{ ref('a') }}"},
                "model.utils.c": {"resource_type": "model", "name": "c", "package_name": "utils"}
            }, "parent_map": {"model.cima.b": ["model.cima.a"]}, "child_map": {"model.cima.a": ["model.cima.b"]}}"#,
        );
        write(
            &root.join("target/compiled/cima/models/marts/a.sql"),
            "select 1 as x\n",
        );
        let project = load(root).unwrap();
        assert_eq!(project.nodes_of(ResourceType::Model).count(), 2);
        let a = &project.manifest.nodes["model.cima.a"];
        let b = &project.manifest.nodes["model.cima.b"];
        assert_eq!(folder_of(a), "marts");
        assert_eq!(folder_of(b), "");
        assert!(matches_filter(a, "tag:ven"));
        assert!(!matches_filter(a, "group:x"));
        assert_eq!(project.preview_sql(a).unwrap().origin, SqlOrigin::Compiled);
        let rendered = project.preview_sql(b).unwrap();
        assert_eq!(rendered.origin, SqlOrigin::Rendered);
        assert_eq!(rendered.sql, "select * from [db].[s].[a]");
        assert_eq!(
            project.lineage("model.cima.a", Direction::Downstream)[0].name,
            "b"
        );
        assert_eq!(
            project
                .lineage_all("model.cima.b", Direction::Downstream)
                .len(),
            0
        );
        // Written after the manifest, so marked as changed since the parse.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(&root.join("models/b.sql"), "select 2");
        assert!(load(root).unwrap().stale.contains("model.cima.b"));
    }

    #[test]
    fn follows_lineage_through_every_step_once() {
        let folder = temp("lineage");
        let root = &folder.0;
        write(&root.join("dbt_project.yml"), "name: p\n");
        // raw -> stg -> mart -> report, raw -> mart directly, and a test on stg that is left out.
        write(
            &root.join("target/manifest.json"),
            r#"{"metadata": {"project_name": "p"}, "nodes": {
                "model.p.stg": {"resource_type": "model", "name": "stg", "package_name": "p"},
                "model.p.mart": {"resource_type": "model", "name": "mart", "package_name": "p"},
                "model.p.report": {"resource_type": "model", "name": "report", "package_name": "p"},
                "test.p.not_null": {"resource_type": "test", "name": "not_null", "package_name": "p"}
            }, "sources": {
                "source.p.erp.raw": {"resource_type": "source", "name": "raw", "source_name": "erp", "package_name": "p"}
            }, "parent_map": {
                "model.p.stg": ["source.p.erp.raw"],
                "model.p.mart": ["model.p.stg", "source.p.erp.raw"],
                "model.p.report": ["model.p.mart"],
                "test.p.not_null": ["model.p.stg"]
            }, "child_map": {
                "model.p.mart": ["model.p.report"],
                "source.p.erp.raw": ["model.p.stg", "model.p.mart"],
                "model.p.stg": ["model.p.mart", "test.p.not_null"]
            }}"#,
        );
        let project = load(root).unwrap();
        let names = |found: Vec<(&Node, usize)>| -> Vec<(String, usize)> {
            found
                .into_iter()
                .map(|(node, distance)| (node.name.clone(), distance))
                .collect()
        };
        assert_eq!(
            names(project.lineage_all("model.p.mart", Direction::Upstream)),
            [("raw".to_owned(), 1), ("stg".to_owned(), 1)]
        );
        assert_eq!(
            names(project.lineage_all("source.p.erp.raw", Direction::Downstream)),
            [
                ("mart".to_owned(), 1),
                ("stg".to_owned(), 1),
                ("report".to_owned(), 2)
            ]
        );
        assert_eq!(
            names(project.lineage_all("model.p.report", Direction::Upstream)),
            [
                ("mart".to_owned(), 1),
                ("raw".to_owned(), 2),
                ("stg".to_owned(), 2)
            ]
        );
        assert_eq!(
            names(project.lineage_all("model.p.stg", Direction::Downstream)),
            [("mart".to_owned(), 1), ("report".to_owned(), 2)]
        );
    }
}
