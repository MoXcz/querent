//! Text logic with no UI or database dependency.

pub mod vim;

use std::ops::Range;

/// Byte ranges of the statements in `text`, trimmed of surrounding whitespace.
///
/// A statement ends at `;`, at a line holding only `GO`, or at a blank line,
/// unless that delimiter is inside a string, quoted identifier, comment, or a
/// dollar-quoted body (`$$ ... $$`, `$tag$ ... $tag$`) as PostgreSQL and
/// Snowflake write function bodies.
/// Comment-only segments are skipped.
#[must_use]
pub fn statements(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut statements = Vec::new();
    let mut segment = Segment::new(0);
    let mut line_start = 0;
    let mut index = 0;

    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'\'' | b'"' => {
                index = skip_quoted(bytes, index, byte);
                segment.has_code = true;
                continue;
            }
            b'[' => {
                index = skip_quoted(bytes, index, b']');
                segment.has_code = true;
                continue;
            }
            b'$' => {
                if let Some(end) = skip_dollar_quoted(bytes, index) {
                    // A blank line or `GO` right after the body still ends the statement.
                    if let Some(newline) = text.get(index..end).and_then(|body| body.rfind('\n')) {
                        line_start = index + newline + 1;
                    }
                    index = end;
                    segment.has_code = true;
                    continue;
                }
                segment.has_code = true;
            }
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index = skip_line_comment(bytes, index);
                continue;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = skip_block_comment(bytes, index);
                continue;
            }
            b';' => {
                segment.finish(text, index, &mut statements);
                segment = Segment::new(index + 1);
            }
            b'\n' => {
                if is_delimiter_line(text.get(line_start..index).unwrap_or_default()) {
                    segment.finish(text, line_start, &mut statements);
                    segment = Segment::new(index + 1);
                }
                line_start = index + 1;
            }
            byte if !byte.is_ascii_whitespace() => segment.has_code = true,
            _ => {}
        }
        index += 1;
    }

    let last_line = text.get(line_start..).unwrap_or_default();
    let end = if is_delimiter_line(last_line) {
        line_start
    } else {
        text.len()
    };
    segment.finish(text, end, &mut statements);
    statements
}

/// The statement to run for a cursor at byte offset `cursor`.
///
/// A cursor inside a statement picks it. A cursor between statements picks the
/// one that ends on the same line before it (the usual spot after typing `;`),
/// then the one that starts on the same line after it.
#[must_use]
pub fn statement_at(text: &str, cursor: usize) -> Option<Range<usize>> {
    let statements = statements(text);
    if let Some(containing) = statements
        .iter()
        .find(|range| range.start <= cursor && cursor <= range.end)
    {
        return Some(containing.clone());
    }
    let same_line = |range: Range<usize>| {
        text.get(range)
            .is_some_and(|between| !between.contains('\n'))
    };
    let previous = statements.iter().rev().find(|range| range.end <= cursor);
    if let Some(previous) = previous.filter(|previous| same_line(previous.end..cursor)) {
        return Some(previous.clone());
    }
    statements
        .iter()
        .find(|range| range.start >= cursor)
        .filter(|next| same_line(cursor..next.start))
        .cloned()
}

struct Segment {
    start: usize,
    has_code: bool,
}

impl Segment {
    fn new(start: usize) -> Self {
        Self {
            start,
            has_code: false,
        }
    }

    fn finish(&self, text: &str, end: usize, statements: &mut Vec<Range<usize>>) {
        if !self.has_code {
            return;
        }
        let Some(raw) = text.get(self.start..end) else {
            return;
        };
        let leading = raw.len() - raw.trim_start().len();
        let trailing = raw.len() - raw.trim_end().len();
        statements.push(self.start + leading..end - trailing);
    }
}

fn is_delimiter_line(line: &str) -> bool {
    let line = line.trim();
    line.is_empty() || line.eq_ignore_ascii_case("go")
}

/// For `$tag$` at `open_index`, the index after the matching closing `$tag$`
/// (or the end of the text); `None` when the `$` does not open a dollar quote,
/// as in `$1`, `$action` or an identifier such as `a$b`.
fn skip_dollar_quoted(bytes: &[u8], open_index: usize) -> Option<usize> {
    let is_word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$';
    if open_index > 0 && bytes.get(open_index - 1).copied().is_some_and(is_word) {
        return None;
    }
    let mut tag_end = open_index + 1;
    while let Some(&byte) = bytes.get(tag_end) {
        if byte == b'$' {
            break;
        }
        let starts_with_digit = tag_end == open_index + 1 && byte.is_ascii_digit();
        if !(byte.is_ascii_alphanumeric() || byte == b'_') || starts_with_digit {
            return None;
        }
        tag_end += 1;
    }
    let delimiter = bytes.get(open_index..=tag_end)?;
    if delimiter.last() != Some(&b'$') {
        return None;
    }
    let body_start = tag_end + 1;
    let close = bytes
        .get(body_start..)?
        .windows(delimiter.len())
        .position(|window| window == delimiter);
    Some(close.map_or(bytes.len(), |offset| body_start + offset + delimiter.len()))
}

/// Returns the index after the closing quote; a doubled closing quote is an escape.
fn skip_quoted(bytes: &[u8], open_index: usize, close: u8) -> usize {
    let mut index = open_index + 1;
    while let Some(&byte) = bytes.get(index) {
        if byte == close {
            if bytes.get(index + 1) == Some(&close) {
                index += 2;
                continue;
            }
            return index + 1;
        }
        index += 1;
    }
    bytes.len()
}

/// Returns the index of the newline that ends the comment, so line tracking sees it.
fn skip_line_comment(bytes: &[u8], start: usize) -> usize {
    bytes
        .get(start..)
        .and_then(|rest| rest.iter().position(|&byte| byte == b'\n'))
        .map_or(bytes.len(), |offset| start + offset)
}

/// SQL Server and PostgreSQL both allow nested block comments.
fn skip_block_comment(bytes: &[u8], start: usize) -> usize {
    let mut depth = 0_usize;
    let mut index = start;
    while index < bytes.len() {
        match (bytes.get(index), bytes.get(index + 1)) {
            (Some(b'/'), Some(b'*')) => {
                depth += 1;
                index += 2;
            }
            (Some(b'*'), Some(b'/')) => {
                depth -= 1;
                index += 2;
                if depth == 0 {
                    return index;
                }
            }
            _ => index += 1,
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_dollar_quoted_bodies_whole() {
        let text = "create function f() returns int as $$\nselect 1;\n\nselect 2;\n$$ language sql;\nselect 3;";
        let found: Vec<&str> = statements(text)
            .into_iter()
            .map(|range| &text[range])
            .collect();
        assert_eq!(
            found,
            vec![
                "create function f() returns int as $$\nselect 1;\n\nselect 2;\n$$ language sql",
                "select 3",
            ]
        );
        let tagged = "do $body$ begin perform 1; end $body$; select $1, a$b from t";
        assert_eq!(statements(tagged).len(), 2);
        let after = "create function f() as $$\nselect 1;\n$$\n\nselect 2";
        assert_eq!(statements(after).len(), 2);
    }

    #[test]
    fn dollar_signs_that_do_not_open_a_quote() {
        for text in [
            "merge t using s on 1 = 1 when matched then delete output $action; select 2",
            "select $partition.pf(x); select 2",
            "select $IDENTITY from t; select 2",
            "select $5; select 2",
            "select a$$b; select 2",
            "select $1,$2; select 2",
        ] {
            assert_eq!(statements(text).len(), 2, "{text}");
        }
    }

    fn texts(text: &str) -> Vec<&str> {
        statements(text)
            .into_iter()
            .map(|range| &text[range])
            .collect()
    }

    fn at(text: &str, cursor: usize) -> Option<&str> {
        statement_at(text, cursor).map(|range| &text[range])
    }

    #[test]
    fn splits_on_semicolons_blank_lines_and_go() {
        let text = "select 1; select 2;\nselect 3\n\nselect 4\nGO\nselect 5\n  go  \nselect 6";
        assert_eq!(
            texts(text),
            [
                "select 1", "select 2", "select 3", "select 4", "select 5", "select 6"
            ]
        );
    }

    #[test]
    fn keeps_multi_line_statements_together() {
        let text = "select a,\n       b\nfrom t\nwhere x = 1";
        assert_eq!(texts(text), [text]);
    }

    #[test]
    fn ignores_delimiters_in_strings_identifiers_and_comments() {
        let text = "select 'a;b', \"c;d\", [e;f], 'it''s;' -- x;y\nfrom t /* g; /* nested; */ h;\n\n */ where 1 = 1";
        assert_eq!(texts(text), [text]);
    }

    #[test]
    fn blank_line_inside_string_does_not_split() {
        let text = "select 'line one\n\nline three'";
        assert_eq!(texts(text), [text]);
    }

    #[test]
    fn skips_comment_only_segments() {
        let text = "-- header\n\nselect 1;\n/* trailing */";
        assert_eq!(texts(text), ["select 1"]);
    }

    #[test]
    fn comment_above_statement_stays_with_it() {
        let text = "-- top customers\nselect * from customers";
        assert_eq!(texts(text), [text]);
    }

    #[test]
    fn go_at_end_without_newline_is_a_delimiter() {
        assert_eq!(texts("select 1\nGO"), ["select 1"]);
    }

    #[test]
    fn unterminated_string_runs_to_end() {
        let text = "select 'oops; select 2";
        assert_eq!(texts(text), [text]);
    }

    #[test]
    fn picks_statement_containing_cursor() {
        let text = "select 1;\nselect 2;\nselect 3;";
        assert_eq!(at(text, 0), Some("select 1"));
        assert_eq!(at(text, 13), Some("select 2"));
        assert_eq!(at(text, text.len() - 1), Some("select 3"));
    }

    #[test]
    fn cursor_after_semicolon_picks_previous_statement() {
        let text = "select 1;  \nselect 2;";
        assert_eq!(at(text, 9), Some("select 1"));
        assert_eq!(at(text, 11), Some("select 1"));
    }

    #[test]
    fn cursor_in_leading_indent_picks_next_statement() {
        let text = "select 1;\n    select 2;";
        assert_eq!(at(text, 11), Some("select 2"));
    }

    #[test]
    fn cursor_on_blank_line_between_statements_picks_nothing() {
        let text = "select 1\n\n\nselect 2";
        assert_eq!(at(text, 9), None);
    }

    #[test]
    fn handles_multibyte_text() {
        let text = "select 'año; ñ' as ñame;\nselect 2";
        assert_eq!(texts(text), ["select 'año; ñ' as ñame", "select 2"]);
    }
}
