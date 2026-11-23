//! The little Jinja the editor understands: `ref()`, `source()`, `config()` and comments.

use std::sync::LazyLock;

use regex::{Captures, Regex};

use crate::manifest::Manifest;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RenderError {
    #[error("ref('{0}') names no model, seed or snapshot in the manifest")]
    UnknownRef(String),
    #[error("source('{0}', '{1}') is not in the manifest")]
    UnknownSource(String, String),
    #[error("{0} has no relation name in the manifest")]
    NoRelation(String),
    /// Macros, `if` blocks or variables that only dbt can render.
    #[error("the model uses Jinja beyond ref() and source(); compile the project first")]
    NeedsCompile,
}

// Patterns are fixed and tested, so building them cannot fail at run time.
#[expect(clippy::expect_used, reason = "fixed patterns, checked by the tests")]
fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("valid pattern")
}

static REF: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r#"\{\{-?\s*ref\(\s*['"]([^'"]+)['"]\s*(?:,\s*['"]([^'"]+)['"]\s*)?(,\s*v(?:ersion)?\s*=\s*[^)]*)?\)\s*-?\}\}"#,
    )
});
static SOURCE: LazyLock<Regex> = LazyLock::new(|| {
    pattern(r#"\{\{-?\s*source\(\s*['"]([^'"]+)['"]\s*,\s*['"]([^'"]+)['"]\s*\)\s*-?\}\}"#)
});
static CONFIG: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?s)\{\{-?\s*config\(.*?\)\s*-?\}\}"));
static COMMENT: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?s)\{#.*?#\}"));
/// What follows a database name in a three-part name: `.schema.`.
static REST_OF_NAME: LazyLock<Regex> =
    LazyLock::new(|| pattern(r#"^\s*\.\s*(?:\[[^\]]*\]|"[^"]*"|\w+)\s*\."#));

/// `raw_code` with `ref()` and `source()` replaced by their relation names.
pub fn render_refs(raw_code: &str, manifest: &Manifest) -> Result<String, RenderError> {
    let mut failure = None;
    let without_comments = COMMENT.replace_all(raw_code, "");
    let without_config = CONFIG.replace_all(&without_comments, "");
    let with_refs = REF.replace_all(&without_config, |captures: &Captures| {
        // A versioned model's relation depends on which version; only dbt knows.
        if captures.get(3).is_some() {
            failure.get_or_insert(RenderError::NeedsCompile);
            return String::new();
        }
        // `ref('package', 'model')` names the package first.
        let (package, name) = match (captures.get(1), captures.get(2)) {
            (Some(package), Some(name)) => (Some(package.as_str()), name.as_str()),
            (Some(name), None) => (None, name.as_str()),
            (None, _) => (None, ""),
        };
        let relation = manifest
            .find_ref(package, name)
            .ok_or_else(|| RenderError::UnknownRef(name.to_owned()))
            .and_then(|node| {
                node.relation_name
                    .clone()
                    .ok_or_else(|| RenderError::NoRelation(node.name.clone()))
            });
        relation.unwrap_or_else(|error| {
            failure.get_or_insert(error);
            String::new()
        })
    });
    let rendered = SOURCE.replace_all(&with_refs, |captures: &Captures| {
        let source_name = captures.get(1).map_or("", |found| found.as_str());
        let table = captures.get(2).map_or("", |found| found.as_str());
        let relation = manifest
            .find_source(source_name, table)
            .ok_or_else(|| RenderError::UnknownSource(source_name.to_owned(), table.to_owned()))
            .and_then(|node| {
                node.relation_name
                    .clone()
                    .ok_or_else(|| RenderError::NoRelation(node.display_name()))
            });
        relation.unwrap_or_else(|error| {
            failure.get_or_insert(error);
            String::new()
        })
    });
    if let Some(error) = failure {
        return Err(error);
    }
    if rendered.contains("{{") || rendered.contains("{%") {
        return Err(RenderError::NeedsCompile);
    }
    Ok(rendered.trim().to_owned())
}

/// `sql` with the database `from` renamed to `to` where it starts a three-part
/// name, `from.schema.table` in any quoting, ignoring case. Strings and comments
/// are left alone, and so are two-part names such as `alias.column`.
#[must_use]
pub fn swap_database(sql: &str, from: &str, to: &str) -> String {
    if from.is_empty() || from.eq_ignore_ascii_case(to) {
        return sql.to_owned();
    }
    let escaped = regex::escape(from);
    let name = pattern(&format!(r#"(?i)\[{escaped}\]|"{escaped}"|\b{escaped}\b"#));
    let skipped = strings_and_comments(sql);
    let mut renamed = String::with_capacity(sql.len());
    let mut copied = 0;
    for found in name.find_iter(sql) {
        let before = sql.get(..found.start()).unwrap_or_default().trim_end();
        let after = sql.get(found.end()..).unwrap_or_default();
        let starts_name = !before.ends_with('.');
        let in_code = !skipped.iter().any(|range| range.contains(&found.start()));
        if !(starts_name && in_code && REST_OF_NAME.is_match(after)) {
            continue;
        }
        renamed.push_str(sql.get(copied..found.start()).unwrap_or_default());
        let (open, close) = match found.as_str().chars().next() {
            Some('[') => ("[", "]"),
            Some('"') => ("\"", "\""),
            _ => ("", ""),
        };
        renamed.push_str(open);
        renamed.push_str(to);
        renamed.push_str(close);
        copied = found.end();
    }
    renamed.push_str(sql.get(copied..).unwrap_or_default());
    renamed
}

/// Byte ranges of `'...'` strings, `--` comments and `/* */` comments.
fn strings_and_comments(sql: &str) -> Vec<std::ops::Range<usize>> {
    let bytes = sql.as_bytes();
    let mut ranges = Vec::new();
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let start = index;
        let end = match (byte, bytes.get(index + 1)) {
            (b'\'', _) => {
                index += 1;
                // `''` inside a string is an escaped quote, not its end.
                loop {
                    match (bytes.get(index), bytes.get(index + 1)) {
                        (Some(b'\''), Some(b'\'')) => index += 2,
                        (Some(b'\''), _) => break index + 1,
                        (Some(_), _) => index += 1,
                        (None, _) => break index,
                    }
                }
            }
            (b'-', Some(b'-')) => sql
                .get(index..)
                .and_then(|rest| rest.find('\n'))
                .map_or(bytes.len(), |newline| index + newline),
            (b'/', Some(b'*')) => sql
                .get(index + 2..)
                .and_then(|rest| rest.find("*/"))
                .map_or(bytes.len(), |close| index + 2 + close + 2),
            _ => {
                index += 1;
                continue;
            }
        };
        ranges.push(start..end);
        index = end.max(start + 1);
    }
    ranges
}

/// What a name typed inside `ref('` or `source('` refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameContext {
    Model,
    SourceGroup,
    /// A table of the named source group.
    SourceTable(String),
}

/// The name being typed before `offset` inside `ref()` or `source()`, and where it starts.
#[must_use]
pub fn name_context(text: &str, offset: usize) -> Option<(NameContext, usize)> {
    static REF_NAME: LazyLock<Regex> =
        LazyLock::new(|| pattern(r#"\bref\(\s*['"]([A-Za-z0-9_]*)$"#));
    static SOURCE_GROUP: LazyLock<Regex> =
        LazyLock::new(|| pattern(r#"\bsource\(\s*['"]([A-Za-z0-9_]*)$"#));
    static SOURCE_TABLE: LazyLock<Regex> = LazyLock::new(|| {
        pattern(r#"\bsource\(\s*['"]([A-Za-z0-9_]+)['"]\s*,\s*['"]([A-Za-z0-9_]*)$"#)
    });
    let before = text.get(..offset)?;
    // Only the current line can hold the call being typed.
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    let line = before.get(line_start..)?;
    let start_of = |captures: &Captures, group: usize| {
        captures.get(group).map(|found| line_start + found.start())
    };
    if let Some(captures) = SOURCE_TABLE.captures(line) {
        let group = captures.get(1)?.as_str().to_owned();
        return Some((NameContext::SourceTable(group), start_of(&captures, 2)?));
    }
    if let Some(captures) = SOURCE_GROUP.captures(line) {
        return Some((NameContext::SourceGroup, start_of(&captures, 1)?));
    }
    let captures = REF_NAME.captures(line)?;
    Some((NameContext::Model, start_of(&captures, 1)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest::parse(
            r#"{"metadata": {"project_name": "cima"}, "nodes": {
                "model.cima.stg_orders": {"resource_type": "model", "name": "stg_orders",
                    "package_name": "cima", "relation_name": "[analytics].[staging].[stg_orders]"},
                "seed.cima.calendar": {"resource_type": "seed", "name": "calendar",
                    "package_name": "cima", "relation_name": "[analytics].[seeds].[calendar]"}
            }, "sources": {
                "source.cima.erp.orders": {"resource_type": "source", "name": "orders",
                    "source_name": "erp", "relation_name": "[raw].[erp].[orders]"}
            }}"#,
        )
        .unwrap()
    }

    #[test]
    fn renders_refs_sources_and_drops_config_and_comments() {
        let raw = "{{ config(materialized='table', tags=['a']) }}\n{# note #}\nselect *\nfrom {{ ref('stg_orders') }} o\njoin {{ref(\"cima\", \"calendar\")}} c on 1 = 1\njoin {{ source('erp', 'orders') }} s on 1 = 1";
        assert_eq!(
            render_refs(raw, &manifest()).unwrap(),
            "select *\nfrom [analytics].[staging].[stg_orders] o\njoin [analytics].[seeds].[calendar] c on 1 = 1\njoin [raw].[erp].[orders] s on 1 = 1"
        );
    }

    #[test]
    fn refuses_what_it_cannot_render() {
        let manifest = manifest();
        assert_eq!(
            render_refs("select {{ dbt_utils.star(ref('stg_orders')) }}", &manifest),
            Err(RenderError::NeedsCompile)
        );
        assert_eq!(
            render_refs("{% if true %}select 1{% endif %}", &manifest),
            Err(RenderError::NeedsCompile)
        );
        assert_eq!(
            render_refs("select * from {{ ref('missing') }}", &manifest),
            Err(RenderError::UnknownRef("missing".to_owned()))
        );
        assert_eq!(
            render_refs("select * from {{ source('erp', 'nope') }}", &manifest),
            Err(RenderError::UnknownSource(
                "erp".to_owned(),
                "nope".to_owned()
            ))
        );
    }

    #[test]
    fn swaps_only_the_qualifying_database() {
        let sql = "select analytics.total, 'analytics.a.b' from [analytics].[staging].[x] join \"Analytics\".s.y on analytics . z . k -- analytics.a.b\njoin analytics.analytics.t";
        assert_eq!(
            swap_database(sql, "analytics", "dev"),
            "select analytics.total, 'analytics.a.b' from [dev].[staging].[x] join \"dev\".s.y on dev . z . k -- analytics.a.b\njoin dev.analytics.t"
        );
        assert_eq!(swap_database(sql, "analytics", "ANALYTICS"), sql);
        assert_eq!(
            swap_database("x 'it''s analytics.a.b'", "analytics", "dev"),
            "x 'it''s analytics.a.b'"
        );
    }

    #[test]
    fn versioned_refs_need_dbt() {
        assert_eq!(
            render_refs("select * from {{ ref('stg_orders', v=2) }}", &manifest()),
            Err(RenderError::NeedsCompile)
        );
    }

    #[test]
    fn finds_the_name_being_typed_in_ref_and_source() {
        let text = "select * from {{ ref('stg_or";
        assert_eq!(
            name_context(text, text.len()),
            Some((NameContext::Model, text.len() - 6))
        );
        let text = "x\n  {{ source(\"erp\", 'ord";
        assert_eq!(
            name_context(text, text.len()),
            Some((NameContext::SourceTable("erp".to_owned()), text.len() - 3))
        );
        let text = "{{ source('";
        assert_eq!(
            name_context(text, text.len()),
            Some((NameContext::SourceGroup, text.len()))
        );
        assert_eq!(name_context("select ref", 10), None);
        assert_eq!(name_context("{{ ref('a') }} b", 16), None);
    }
}
