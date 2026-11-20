//! Built-in SQL formatting with `sqlformat`, refusing any result that changes more than layout.

use serde::{Deserialize, Serialize};

/// How keywords are written after formatting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeywordCase {
    #[default]
    Upper,
    Lower,
    /// As typed.
    Preserve,
}

/// The grammar quirks the formatter must respect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatDialect {
    /// `[bracketed]` names and `@variables`.
    SqlServer,
    /// Arrays and `::` casts.
    Postgres,
    Generic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FormatStyle {
    pub indent: u8,
    pub keyword_case: KeywordCase,
    pub lines_between_statements: u8,
}

impl Default for FormatStyle {
    fn default() -> Self {
        Self {
            indent: 4,
            keyword_case: KeywordCase::Upper,
            lines_between_statements: 1,
        }
    }
}

/// `text` formatted, or `None` when formatting would change anything but
/// whitespace and keyword case, which means the formatter misread the SQL.
#[must_use]
pub fn format_sql(text: &str, dialect: FormatDialect, style: &FormatStyle) -> Option<String> {
    let options = sqlformat::FormatOptions {
        indent: sqlformat::Indent::Spaces(style.indent.clamp(1, 8)),
        uppercase: match style.keyword_case {
            KeywordCase::Upper => Some(true),
            KeywordCase::Lower => Some(false),
            KeywordCase::Preserve => None,
        },
        lines_between_queries: style.lines_between_statements.clamp(1, 3),
        dialect: match dialect {
            FormatDialect::SqlServer => sqlformat::Dialect::SQLServer,
            FormatDialect::Postgres => sqlformat::Dialect::PostgreSql,
            FormatDialect::Generic => sqlformat::Dialect::Generic,
        },
        ..sqlformat::FormatOptions::default()
    };
    let core = text.trim();
    if core.is_empty() {
        return Some(text.to_owned());
    }
    let (protected, bodies, prefix) = protect_dollar_bodies(core);
    let formatted = sqlformat::format(&protected, &sqlformat::QueryParams::None, &options);
    let formatted = formatted.trim();
    // Compared with bodies still swapped out, so their contents are never scanned.
    if signature(&protected, dialect) != signature(formatted, dialect) {
        return None;
    }
    let restored = restore_dollar_bodies(formatted, &bodies, &prefix)?;
    // Keep the selection's surrounding whitespace, such as its indent and blank lines.
    let leading = text.len() - text.trim_start().len();
    let trailing_start = text.trim_end().len();
    Some(format!(
        "{}{restored}{}",
        text.get(..leading).unwrap_or_default(),
        text.get(trailing_start..).unwrap_or_default()
    ))
}

/// `$$ ... $$` bodies swapped for string-literal placeholders, so the
/// formatter leaves them alone; their language may care about whitespace.
/// The placeholder prefix is one the text does not already contain.
fn protect_dollar_bodies(text: &str) -> (String, Vec<String>, String) {
    let mut prefix = "sql_editor_body".to_owned();
    while text.contains(&prefix) {
        prefix.push('_');
    }
    let mut protected = String::with_capacity(text.len());
    let mut bodies = Vec::new();
    let mut last = 0;
    for range in crate::dollar_quoted_ranges(text) {
        let (Some(before), Some(body)) = (text.get(last..range.start), text.get(range.clone()))
        else {
            continue;
        };
        protected.push_str(before);
        protected.push_str(&placeholder(&prefix, bodies.len()));
        bodies.push(body.to_owned());
        last = range.end;
    }
    protected.push_str(text.get(last..).unwrap_or_default());
    (protected, bodies, prefix)
}

fn placeholder(prefix: &str, index: usize) -> String {
    format!("'{prefix}_{index}_'")
}

/// `None` if a placeholder went missing, which would lose a body.
fn restore_dollar_bodies(formatted: &str, bodies: &[String], prefix: &str) -> Option<String> {
    let mut restored = formatted.to_owned();
    // From the last, so `_1_` is never found inside a body restored before it.
    for (index, body) in bodies.iter().enumerate().rev() {
        let marker = placeholder(prefix, index);
        if !restored.contains(&marker) {
            return None;
        }
        restored = restored.replacen(&marker, body, 1);
    }
    Some(restored)
}

/// The text with whitespace outside quotes dropped and the rest lowercased,
/// except quoted text, which must match exactly. Equal signatures mean the
/// formatter changed only layout and keyword case.
fn signature(text: &str, dialect: FormatDialect) -> String {
    let mut signature = String::with_capacity(text.len());
    let mut quote: Option<char> = None;
    let mut characters = text.chars().peekable();
    // Backslash escapes quotes in PostgreSQL `E'...'` strings, not in T-SQL.
    let backslash_escapes = dialect != FormatDialect::SqlServer;
    while let Some(character) = characters.next() {
        match quote {
            Some(close) => {
                signature.push(character);
                if character == '\\' && backslash_escapes {
                    if let Some(escaped) = characters.next() {
                        signature.push(escaped);
                    }
                } else if character == close {
                    quote = None;
                }
            }
            None => match character {
                '\'' | '"' | '`' => {
                    quote = Some(character);
                    signature.push(character);
                }
                '[' if dialect == FormatDialect::SqlServer => {
                    quote = Some(']');
                    signature.push(character);
                }
                // Comments keep their words but not their spacing, which the formatter may move.
                '-' if characters.peek() == Some(&'-') => {
                    for comment in characters.by_ref() {
                        if comment == '\n' {
                            break;
                        }
                        if !comment.is_whitespace() {
                            signature.extend(comment.to_lowercase());
                        }
                    }
                }
                '/' if characters.peek() == Some(&'*') => {
                    let mut previous = '/';
                    for comment in characters.by_ref() {
                        if !comment.is_whitespace() {
                            signature.extend(comment.to_lowercase());
                        }
                        if previous == '*' && comment == '/' {
                            break;
                        }
                        previous = comment;
                    }
                }
                character if character.is_whitespace() => {}
                character => signature.extend(character.to_lowercase()),
            },
        }
    }
    signature
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upper(text: &str, dialect: FormatDialect) -> String {
        format_sql(text, dialect, &FormatStyle::default()).unwrap()
    }

    #[test]
    fn formats_a_select() {
        let formatted = upper(
            "select a, b from t join u on u.id = t.id where a = 1 order by b",
            FormatDialect::Generic,
        );
        assert!(
            formatted.starts_with("SELECT\n    a,\n    b\nFROM"),
            "{formatted}"
        );
        assert!(formatted.contains("WHERE\n    a = 1"), "{formatted}");
    }

    #[test]
    fn keeps_tsql_names_variables_and_strings() {
        let text = "declare @x int = 1; select top 10 [order id], N'it''s' from [dbo].[my table] where x = @x";
        let formatted = upper(text, FormatDialect::SqlServer);
        assert!(formatted.contains("[order id]"), "{formatted}");
        assert!(formatted.contains("[dbo].[my table]"), "{formatted}");
        assert!(formatted.contains("@x"), "{formatted}");
        assert!(formatted.contains("N'it''s'"), "{formatted}");
    }

    #[test]
    fn keeps_postgres_casts_and_bodies() {
        let text = "select a::int from t; create function f() returns int as $body$\n  select   1;\n$body$ language plpython3u;";
        let formatted = upper(text, FormatDialect::Postgres);
        assert!(formatted.contains("a::int"), "{formatted}");
        assert!(
            formatted.contains("$body$\n  select   1;\n$body$"),
            "{formatted}"
        );
    }

    #[test]
    fn lowercase_and_preserve() {
        let lower = FormatStyle {
            keyword_case: KeywordCase::Lower,
            ..FormatStyle::default()
        };
        assert!(
            format_sql("SELECT 1", FormatDialect::Generic, &lower)
                .unwrap()
                .starts_with("select")
        );
        let preserve = FormatStyle {
            keyword_case: KeywordCase::Preserve,
            ..FormatStyle::default()
        };
        assert!(
            format_sql("Select 1", FormatDialect::Generic, &preserve)
                .unwrap()
                .starts_with("Select")
        );
    }

    #[test]
    fn a_changed_meaning_is_refused() {
        let same = |a: &str, b: &str| {
            signature(a, FormatDialect::Generic) == signature(b, FormatDialect::Generic)
        };
        assert!(!same("select 'a b'", "select 'ab'"));
        assert!(same("select  a\nfrom t", "SELECT a FROM t"));
    }

    #[test]
    fn comments_and_escapes_do_not_break_the_check() {
        let formatted = upper(
            "select a /* don't */ from t -- it's here\nwhere b = E'a\\'b'",
            FormatDialect::Postgres,
        );
        assert!(formatted.contains("/* don't */"), "{formatted}");
        assert!(formatted.contains("E'a\\'b'"), "{formatted}");
    }

    #[test]
    fn placeholder_names_in_user_text_are_left_alone() {
        let text = "select 'sql_editor_body_0_' as x; create function f() returns int as $$ select 1 $$ language sql;";
        let formatted = upper(text, FormatDialect::Postgres);
        assert!(formatted.contains("'sql_editor_body_0_'"), "{formatted}");
        assert!(formatted.contains("$$ select 1 $$"), "{formatted}");
    }

    #[test]
    fn brackets_outside_sql_server_and_surrounding_whitespace() {
        let formatted = upper("select [MyCol] from t", FormatDialect::Generic);
        assert!(formatted.to_lowercase().contains("mycol"), "{formatted}");
        let indented = upper("    select 1\n\n", FormatDialect::Generic);
        assert!(indented.starts_with("    SELECT"), "{indented:?}");
        assert!(indented.ends_with("\n\n"), "{indented:?}");
    }
}
