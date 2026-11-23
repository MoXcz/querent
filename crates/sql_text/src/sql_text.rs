//! Text logic with no UI or database dependency.

pub mod completion;
pub mod format;
pub mod read_only;
pub mod syntax;
pub mod vim;

use std::ops::Range;

/// What ends a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delimiters {
    /// `;`, a line holding only `GO`, or a blank line: what the editor runs under the cursor.
    Editor,
    /// `;` only: what a server takes as one statement.
    Semicolon,
    /// A line holding only `GO`: the batches SQL Server tools send one by one.
    GoLine,
}

impl Delimiters {
    fn splits_at_semicolon(self) -> bool {
        match self {
            Self::Editor | Self::Semicolon => true,
            Self::GoLine => false,
        }
    }

    fn splits_at_line(self, line: &str) -> bool {
        let line = line.trim();
        match self {
            Self::Editor => line.is_empty() || line.eq_ignore_ascii_case("go"),
            Self::Semicolon => false,
            Self::GoLine => line.eq_ignore_ascii_case("go"),
        }
    }
}

/// Byte ranges of the statements in `text`, trimmed of surrounding whitespace.
///
/// A statement ends at `;`, at a line holding only `GO`, or at a blank line,
/// unless that delimiter is inside a string, quoted identifier, comment, or a
/// dollar-quoted body (`$$ ... $$`, `$tag$ ... $tag$`) as PostgreSQL and
/// Snowflake write function bodies.
/// Comment-only segments are skipped.
#[must_use]
pub fn statements(text: &str) -> Vec<Range<usize>> {
    split(text, Delimiters::Editor)
}

/// [`statements`] with a chosen set of delimiters.
#[must_use]
pub fn split(text: &str, delimiters: Delimiters) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut statements = Vec::new();
    let mut segment = Segment::new(0);
    let mut line_start = 0;
    let mut index = 0;

    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'\'' | b'"' => {
                index = skip_quoted(bytes, index, byte).0;
                segment.has_code = true;
                continue;
            }
            b'[' => {
                index = skip_quoted(bytes, index, b']').0;
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
            b';' if delimiters.splits_at_semicolon() => {
                segment.finish(text, index, &mut statements);
                segment = Segment::new(index + 1);
            }
            b'\n' => {
                if delimiters.splits_at_line(text.get(line_start..index).unwrap_or_default()) {
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
    let end = if delimiters.splits_at_line(last_line) {
        line_start
    } else {
        text.len()
    };
    segment.finish(text, end, &mut statements);
    statements
}

/// Why a file cannot be opened without changing its content on the next save.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("UTF-32 files are not supported")]
    Utf32,
    #[error("the file starts as UTF-16 but is not valid UTF-16")]
    BrokenUtf16,
    #[error("the file starts as UTF-8 but is not valid UTF-8")]
    BrokenUtf8,
}

/// The text of a `.sql` file. A byte order mark picks UTF-8 or UTF-16 (as SQL
/// Server Management Studio saves); without one the file is UTF-8, or Windows-1252
/// when it is not valid UTF-8, so no character is ever replaced.
pub fn decode_file(bytes: &[u8]) -> Result<String, DecodeError> {
    let utf16 = |rest: &[u8], unit: fn([u8; 2]) -> u16| {
        let (pairs, leftover) = rest.as_chunks::<2>();
        if !leftover.is_empty() {
            return Err(DecodeError::BrokenUtf16);
        }
        char::decode_utf16(pairs.iter().copied().map(unit))
            .collect::<Result<String, _>>()
            .map_err(|_| DecodeError::BrokenUtf16)
    };
    match bytes {
        [0xFF, 0xFE, 0, 0, ..] | [0, 0, 0xFE, 0xFF, ..] => Err(DecodeError::Utf32),
        [0xEF, 0xBB, 0xBF, rest @ ..] => std::str::from_utf8(rest)
            .map(str::to_owned)
            .map_err(|_| DecodeError::BrokenUtf8),
        [0xFF, 0xFE, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, u16::from_be_bytes),
        _ => {
            Ok(std::str::from_utf8(bytes)
                .map_or_else(|_| decode_windows_1252(bytes), str::to_owned))
        }
    }
}

/// Every byte is a character in Windows-1252, the "ANSI" encoding of older scripts.
fn decode_windows_1252(bytes: &[u8]) -> String {
    const HIGH: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž',
        '\u{8f}', '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}',
        'ž', 'Ÿ',
    ];
    bytes
        .iter()
        .map(|&byte| match byte {
            0x80..=0x9F => HIGH
                .get(usize::from(byte - 0x80))
                .copied()
                .unwrap_or(char::REPLACEMENT_CHARACTER),
            _ => char::from(byte),
        })
        .collect()
}

/// A number that changes when the text does, however the text is cut into
/// pieces (FNV-1a); tells whether a tab differs from its file without keeping a copy.
#[must_use]
pub fn fingerprint<'a>(pieces: impl IntoIterator<Item = &'a str>) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in pieces.into_iter().flat_map(str::bytes) {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The text a cut removes for a cursor at `cursor` with nothing selected:
/// its whole line and line break, or for a last line without one, the line
/// break before it, so no empty line is left behind.
#[must_use]
pub fn line_to_cut(text: &str, cursor: usize) -> Range<usize> {
    let line = line_at(text, cursor);
    if line.end == text.len() && !text.ends_with('\n') && line.start > 0 {
        let crlf = text
            .get(..line.start)
            .is_some_and(|before| before.ends_with("\r\n"));
        line.start - if crlf { 2 } else { 1 }..line.end
    } else {
        line
    }
}

/// The line `cursor` is on, from its start through its line break if it has one.
#[must_use]
pub fn line_at(text: &str, cursor: usize) -> Range<usize> {
    let cursor = cursor.min(text.len());
    let before = text.get(..cursor).unwrap_or_default();
    let start = before.rfind('\n').map_or(0, |index| index + 1);
    let after = text.get(cursor..).unwrap_or_default();
    let end = after
        .find('\n')
        .map_or(text.len(), |index| cursor + index + 1);
    start..end
}

/// The clipboard text for copying or cutting `line`: always ends in a line break.
#[must_use]
pub fn line_clip(text: &str, line: Range<usize>) -> String {
    let line = text.get(line).unwrap_or_default();
    let line = line
        .strip_prefix("\r\n")
        .or_else(|| line.strip_prefix('\n'))
        .unwrap_or(line);
    if line.ends_with('\n') {
        line.to_owned()
    } else {
        format!("{line}\n")
    }
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

/// Byte ranges of the `$tag$ ... $tag$` bodies in `text`, skipping strings and comments.
pub(crate) fn dollar_quoted_ranges(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        index = match byte {
            b'\'' | b'"' => skip_quoted(bytes, index, byte).0,
            b'-' if bytes.get(index + 1) == Some(&b'-') => skip_line_comment(bytes, index),
            b'/' if bytes.get(index + 1) == Some(&b'*') => skip_block_comment(bytes, index),
            b'$' => match skip_dollar_quoted(bytes, index) {
                Some(end) => {
                    ranges.push(index..end);
                    end
                }
                None => index + 1,
            },
            _ => index + 1,
        };
    }
    ranges
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

/// The index after the closing quote and whether it was found; a doubled closing quote is an escape.
pub(crate) fn skip_quoted(bytes: &[u8], open_index: usize, close: u8) -> (usize, bool) {
    let mut index = open_index + 1;
    while let Some(&byte) = bytes.get(index) {
        if byte == close {
            if bytes.get(index + 1) == Some(&close) {
                index += 2;
                continue;
            }
            return (index + 1, true);
        }
        index += 1;
    }
    (bytes.len(), false)
}

/// Returns the index of the newline that ends the comment, so line tracking sees it.
pub(crate) fn skip_line_comment(bytes: &[u8], start: usize) -> usize {
    bytes
        .get(start..)
        .and_then(|rest| rest.iter().position(|&byte| byte == b'\n'))
        .map_or(bytes.len(), |offset| start + offset)
}

/// SQL Server and PostgreSQL both allow nested block comments.
pub(crate) fn skip_block_comment(bytes: &[u8], start: usize) -> usize {
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
    fn copies_and_cuts_whole_lines() {
        let text = "select 1\nfrom t\nwhere x";
        assert_eq!(line_at(text, 3), 0..9);
        assert_eq!(line_at(text, 9), 9..16);
        assert_eq!(line_at(text, text.len()), 16..23);
        assert_eq!(line_clip(text, line_at(text, 3)), "select 1\n");
        assert_eq!(line_clip(text, line_at(text, 20)), "where x\n");
        // The last line takes the break before it, so cutting leaves no empty line.
        assert_eq!(line_to_cut(text, 20), 15..23);
        assert_eq!(line_clip(text, line_to_cut(text, 20)), "where x\n");
        assert_eq!(line_to_cut(text, 3), 0..9);
        assert_eq!(line_to_cut("only", 2), 0..4);
        assert_eq!(line_at("", 0), 0..0);
        let crlf = "select 1\r\nwhere x";
        assert_eq!(line_to_cut(crlf, 12), 8..17);
        assert_eq!(line_clip(crlf, line_to_cut(crlf, 12)), "where x\n");
    }

    #[test]
    fn a_cursor_inside_a_multibyte_character_or_past_the_end_is_safe() {
        let text = "select 'é';\r\nselect 2";
        let inside = text.find('é').unwrap() + 1;
        assert_eq!(statement_at(text, inside), Some(0..11));
        assert_eq!(statement_at(text, text.len() + 10), None);
        assert_eq!(statement_at("", 0), None);
    }

    #[test]
    fn decodes_files_without_replacing_characters() {
        assert_eq!(decode_file(b"select 1").unwrap(), "select 1");
        assert_eq!(decode_file(b"\xEF\xBB\xBFselect 1").unwrap(), "select 1");
        assert_eq!(decode_file(b"\xFF\xFEs\0e\0\xE9\0").unwrap(), "seé");
        assert_eq!(decode_file(b"\xFE\xFF\0s\0e").unwrap(), "se");
        assert_eq!(decode_file(b"").unwrap(), "");
        // Not UTF-8, so Windows-1252: `é` and the euro sign.
        assert_eq!(decode_file(b"caf\xE9 \x80").unwrap(), "café €");
        assert_eq!(decode_file(b"\xFF\xFEs\0e"), Err(DecodeError::BrokenUtf16));
        assert_eq!(
            decode_file(b"\xFF\xFE\x00\xD8"),
            Err(DecodeError::BrokenUtf16)
        );
        assert_eq!(decode_file(b"\xFF\xFE\0\0s\0\0\0"), Err(DecodeError::Utf32));
        assert_eq!(
            decode_file(b"\xEF\xBB\xBF\xFF"),
            Err(DecodeError::BrokenUtf8)
        );
    }

    #[test]
    fn a_fingerprint_ignores_how_the_text_is_cut() {
        assert_eq!(fingerprint(["select 1"]), fingerprint(["sel", "", "ect 1"]));
        assert_ne!(fingerprint(["select 1"]), fingerprint(["select 2"]));
    }

    #[test]
    fn splits_by_the_chosen_delimiters() {
        let found = |text: &'static str, delimiters| -> Vec<&'static str> {
            split(text, delimiters)
                .into_iter()
                .map(|range| &text[range])
                .collect()
        };
        let delete = "delete from t\n\nwhere id = 1;\nselect 1";
        assert_eq!(
            found(delete, Delimiters::Semicolon),
            vec!["delete from t\n\nwhere id = 1", "select 1"]
        );
        assert_eq!(found(delete, Delimiters::Editor).len(), 3);
        let script = "select 1;\nselect 'go\nGO\n';\nGO\n\nselect 2\n  go  \n";
        assert_eq!(
            found(script, Delimiters::GoLine),
            vec!["select 1;\nselect 'go\nGO\n';", "select 2"]
        );
    }

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
