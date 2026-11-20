//! Syntax errors from the tree-sitter SQL grammar (`tree-sitter-sequel`).
//!
//! The grammar does not know T-SQL (`TOP`, `[names]`, `GO`, `DECLARE`), so
//! callers only ask for errors on PostgreSQL, SQLite and Snowflake text.

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    pub range: Range<usize>,
    pub message: String,
}

/// At most this many errors; one typo can make the grammar flag everything after it.
const MAX_ERRORS: usize = 20;

/// Errors in `text`, in order. Statements are parsed one at a time, so an
/// error in one does not spill into the next.
#[must_use]
pub fn syntax_errors(text: &str) -> Vec<SyntaxError> {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_sequel::LANGUAGE.into())
        .is_err()
    {
        return Vec::new();
    }
    let mut errors = Vec::new();
    for statement in crate::statements(text) {
        let Some(source) = text.get(statement.clone()) else {
            continue;
        };
        let Some(tree) = parser.parse(source, None) else {
            continue;
        };
        collect(tree.root_node(), statement.start, &mut errors);
        if errors.len() >= MAX_ERRORS {
            break;
        }
    }
    errors.truncate(MAX_ERRORS);
    errors
}

fn collect(node: tree_sitter::Node<'_>, base: usize, errors: &mut Vec<SyntaxError>) {
    if node.is_missing() {
        errors.push(SyntaxError {
            range: base + node.start_byte()..base + node.end_byte().max(node.start_byte() + 1),
            message: format!("missing {}", node.kind()),
        });
        return;
    }
    if node.is_error() {
        errors.push(SyntaxError {
            range: base + node.start_byte()..base + node.end_byte(),
            message: "syntax error".to_owned(),
        });
        // The children of an error node are part of the same mistake.
        return;
    }
    if !node.has_error() {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, base, errors);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_statements_have_no_errors() {
        assert_eq!(
            syntax_errors("select a, b from t where x = 1;\nupdate t set a = 1;"),
            vec![]
        );
    }

    #[test]
    fn reports_a_typo_in_its_statement_only() {
        let text = "select 1;\nselct * from t;\nselect 2;";
        let errors = syntax_errors(text);
        assert_eq!(errors.len(), 1, "{errors:?}");
        let error = &errors[0];
        assert!(text[error.range.clone()].contains("selct"));
    }
}
