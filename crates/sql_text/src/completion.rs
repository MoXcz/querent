//! What to complete at the cursor, from a small SQL tokenizer.
//!
//! A tokenizer, not the tree-sitter grammar: the statement being typed is
//! usually incomplete, and the grammar rejects T-SQL (`TOP`, `[names]`).

use std::ops::Range;

/// Keywords offered where no table or column fits.
pub const KEYWORDS: &[&str] = &[
    "select",
    "from",
    "where",
    "join",
    "inner join",
    "left join",
    "right join",
    "full join",
    "cross join",
    "on",
    "and",
    "or",
    "not",
    "in",
    "exists",
    "between",
    "like",
    "is null",
    "is not null",
    "group by",
    "order by",
    "having",
    "limit",
    "top",
    "distinct",
    "as",
    "case",
    "when",
    "then",
    "else",
    "end",
    "union",
    "union all",
    "insert into",
    "values",
    "update",
    "set",
    "delete from",
    "with",
    "create",
    "alter",
    "drop",
    "table",
    "view",
    "asc",
    "desc",
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "cast",
    "coalesce",
];

/// Words that end a table reference, so they are never read as an alias.
const RESERVED: &[&str] = &[
    "where",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "outer",
    "cross",
    "on",
    "group",
    "order",
    "having",
    "limit",
    "union",
    "set",
    "values",
    "select",
    "from",
    "as",
    "using",
    "natural",
    "lateral",
    "with",
    "into",
    "when",
    "then",
    "else",
    "end",
    "and",
    "or",
    "not",
    "qualify",
    "window",
    "fetch",
    "offset",
    "for",
    "option",
    "pivot",
    "unpivot",
    "returning",
    "output",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
    pub alias: Option<String>,
}

impl TableRef {
    /// Whether `word` names this table in the query: its alias, or its name when it has none.
    #[must_use]
    pub fn is_called(&self, word: &str) -> bool {
        match &self.alias {
            Some(alias) => alias.eq_ignore_ascii_case(word),
            None => self.name.eq_ignore_ascii_case(word),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expectation {
    /// After `from`, `join`, `into`, `update`; `schema` when typed as `schema.`.
    Table {
        schema: Option<String>,
    },
    /// In a select list, `where`, `on`, `set`, `group by`, `order by`;
    /// `qualifier` when typed as `alias.`.
    Column {
        qualifier: Option<String>,
    },
    Keyword,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionContext {
    pub expectation: Expectation,
    /// The partial word before the cursor; a completion replaces it.
    pub replace: Range<usize>,
    pub prefix: String,
    /// Tables referenced anywhere in the statement, with their aliases.
    pub tables: Vec<TableRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kind {
    /// An identifier or keyword, unquoted.
    Word(String),
    Dot,
    Comma,
    Open,
    Close,
    Other,
}

#[derive(Debug, Clone)]
pub(crate) struct Token {
    pub(crate) kind: Kind,
    pub(crate) range: Range<usize>,
}

impl Token {
    pub(crate) fn word(&self) -> Option<&str> {
        match &self.kind {
            Kind::Word(word) => Some(word),
            Kind::Dot | Kind::Comma | Kind::Open | Kind::Close | Kind::Other => None,
        }
    }

    pub(crate) fn is_keyword(&self, keyword: &str) -> bool {
        self.word()
            .is_some_and(|word| word.eq_ignore_ascii_case(keyword))
    }
}

/// The completion context for a cursor at byte offset `cursor`.
#[must_use]
pub fn context_at(text: &str, cursor: usize) -> CompletionContext {
    let cursor = cursor.min(text.len());
    let scope = statement_scope(text, cursor);
    let statement = text.get(scope.clone()).unwrap_or_default();
    let tokens = tokenize(statement, scope.start);

    let prefix_start = text
        .get(..cursor)
        .unwrap_or_default()
        .char_indices()
        .rev()
        .take_while(|(_, character)| is_identifier_char(*character))
        .last()
        .map_or(cursor, |(index, _)| index);

    let prefix = text
        .get(prefix_start..cursor)
        .unwrap_or_default()
        .to_owned();
    // A completion is inserted quoted when it needs it, so it replaces a
    // quote the user already opened, and the matching close after the cursor.
    let opening = text
        .get(..prefix_start)
        .and_then(|before| before.chars().next_back())
        .filter(|character| matches!(character, '[' | '"' | '`'));
    let replace_start = prefix_start - opening.map_or(0, char::len_utf8);
    let closing = opening.map(|open| if open == '[' { ']' } else { open });
    let replace_end = match (
        closing,
        text.get(cursor..).and_then(|after| after.chars().next()),
    ) {
        (Some(close), Some(next)) if next == close => cursor + close.len_utf8(),
        _ => cursor,
    };
    let replace = replace_start..replace_end;

    let before: Vec<&Token> = tokens
        .iter()
        .filter(|token| token.range.end <= replace_start)
        .collect();
    let qualifier = match before.as_slice() {
        [.., name, dot] if dot.kind == Kind::Dot && dot.range.end == replace_start => {
            name.word().map(ToOwned::to_owned)
        }
        _ => None,
    };
    let expectation = match (clause_before(&before), qualifier) {
        (Clause::Table, schema) => {
            let after_name = before
                .last()
                .is_some_and(|token| token.word().is_some() && !is_clause_word(token));
            if after_name && schema.is_none() {
                Expectation::Keyword
            } else {
                Expectation::Table { schema }
            }
        }
        (Clause::Column, qualifier) => Expectation::Column { qualifier },
        (Clause::None, Some(qualifier)) => Expectation::Column {
            qualifier: Some(qualifier),
        },
        (Clause::None, None) => Expectation::Keyword,
    };
    CompletionContext {
        expectation,
        replace,
        prefix,
        tables: table_refs(&tokens),
    }
}

/// The dotted name around `offset` (`alias.column`, `schema.table`), split
/// into its parts, with the range of the part under the offset.
#[must_use]
pub fn name_at(text: &str, offset: usize) -> Option<(Vec<String>, Range<usize>)> {
    let scope = statement_scope(text, offset);
    let statement = text.get(scope.clone())?;
    let tokens = tokenize(statement, scope.start);
    let index = tokens.iter().position(|token| {
        token.range.start <= offset && offset <= token.range.end && token.word().is_some()
    })?;
    let mut start = index;
    while start >= 2
        && tokens
            .get(start - 1)
            .is_some_and(|token| token.kind == Kind::Dot)
        && tokens
            .get(start - 2)
            .is_some_and(|token| token.word().is_some())
    {
        start -= 2;
    }
    let parts = tokens
        .get(start..=index)?
        .iter()
        .filter_map(|token| token.word().map(ToOwned::to_owned))
        .collect();
    Some((parts, tokens.get(index)?.range.clone()))
}

/// Tables referenced by the statement around `offset`, with their aliases.
#[must_use]
pub fn tables_at(text: &str, offset: usize) -> Vec<TableRef> {
    let scope = statement_scope(text, offset);
    let statement = text.get(scope.clone()).unwrap_or_default();
    table_refs(&tokenize(statement, scope.start))
}

enum Clause {
    Table,
    Column,
    None,
}

/// The clause the cursor is in: the last clause keyword before it, skipping
/// closed parentheses so a finished subquery does not count.
fn clause_before(before: &[&Token]) -> Clause {
    let mut depth = 0_usize;
    for token in before.iter().rev() {
        match token.kind {
            Kind::Close => depth += 1,
            Kind::Open => depth = depth.saturating_sub(1),
            Kind::Word(_) if depth == 0 => {
                let word = token.word().unwrap_or_default().to_ascii_lowercase();
                match word.as_str() {
                    "from" | "join" | "into" | "update" | "table" | "truncate" => {
                        return Clause::Table;
                    }
                    "select" | "where" | "on" | "by" | "having" | "set" | "and" | "or" | "when"
                    | "then" | "else" | "case" | "distinct" | "qualify" | "using" => {
                        return Clause::Column;
                    }
                    "values" | "limit" | "offset" | "union" | "intersect" | "except" => {
                        return Clause::None;
                    }
                    _ => {}
                }
            }
            Kind::Word(_) | Kind::Dot | Kind::Comma | Kind::Other => {}
        }
    }
    Clause::None
}

fn is_clause_word(token: &Token) -> bool {
    ["from", "join", "into", "update", "table", "truncate"]
        .iter()
        .any(|keyword| token.is_keyword(keyword))
}

/// `from a`, `join schema.b as x`, `update c`, `into d`, and comma lists after `from`.
fn table_refs(tokens: &[Token]) -> Vec<TableRef> {
    let mut tables = Vec::new();
    let mut index = 0;
    while let Some(token) = tokens.get(index) {
        index += 1;
        let lists = token.is_keyword("from");
        if !(lists
            || token.is_keyword("join")
            || token.is_keyword("update")
            || token.is_keyword("into"))
        {
            continue;
        }
        while let Some((table, next)) = table_ref(tokens, index) {
            tables.push(table);
            index = next;
            let comma = tokens
                .get(index)
                .is_some_and(|token| token.kind == Kind::Comma);
            if !(lists && comma) {
                break;
            }
            index += 1;
        }
    }
    tables
}

fn table_ref(tokens: &[Token], start: usize) -> Option<(TableRef, usize)> {
    let mut parts = vec![tokens.get(start)?.word()?.to_owned()];
    if parts.first().is_some_and(|word| is_reserved(word)) {
        return None;
    }
    let mut index = start + 1;
    while tokens
        .get(index)
        .is_some_and(|token| token.kind == Kind::Dot)
    {
        let Some(word) = tokens.get(index + 1).and_then(Token::word) else {
            break;
        };
        parts.push(word.to_owned());
        index += 2;
    }
    let name = parts.pop()?;
    let schema = parts.pop();
    if tokens
        .get(index)
        .is_some_and(|token| token.is_keyword("as"))
    {
        index += 1;
    }
    let alias = match tokens.get(index).and_then(Token::word) {
        Some(word) if !is_reserved(word) => {
            index += 1;
            Some(word.to_owned())
        }
        Some(_) | None => None,
    };
    Some((
        TableRef {
            schema,
            name,
            alias,
        },
        index,
    ))
}

fn is_reserved(word: &str) -> bool {
    RESERVED
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(word))
}

/// The statement holding `cursor`, extended to the cursor while it is being typed.
fn statement_scope(text: &str, cursor: usize) -> Range<usize> {
    let statements = crate::statements(text);
    match statements.iter().rev().find(|range| range.start <= cursor) {
        Some(range) if cursor <= range.end => range.clone(),
        Some(range) => {
            // Past the end: the cursor starts a new statement unless only
            // spaces separate it from the previous one.
            let between = text.get(range.end..cursor).unwrap_or_default();
            if between.contains(';') || between.contains("\n\n") {
                let start = range.end + between.rfind(';').map_or(0, |offset| offset + 1);
                start..cursor
            } else {
                range.start..cursor
            }
        }
        None => 0..cursor,
    }
}

fn is_identifier_char(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '$' | '@' | '#')
}

pub(crate) fn tokenize(text: &str, base: usize) -> Vec<Token> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    let push = |tokens: &mut Vec<Token>, kind: Kind, range: Range<usize>| {
        tokens.push(Token {
            kind,
            range: base + range.start..base + range.end,
        });
    };
    while let Some(&byte) = bytes.get(index) {
        let start = index;
        match byte {
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index = crate::skip_line_comment(bytes, index);
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = crate::skip_block_comment(bytes, index);
            }
            b'\'' => {
                index = crate::skip_quoted(bytes, index, b'\'').0;
                push(&mut tokens, Kind::Other, start..index);
            }
            b'"' | b'[' | b'`' => {
                let close = match byte {
                    b'[' => b']',
                    other => other,
                };
                let (end, closed) = crate::skip_quoted(bytes, index, close);
                index = end;
                let inner_end = if closed { index - 1 } else { index };
                let inner = text.get(start + 1..inner_end).unwrap_or_default();
                let doubled = [close, close];
                let unescaped = inner.replace(
                    std::str::from_utf8(&doubled).unwrap_or_default(),
                    std::str::from_utf8(&[close]).unwrap_or_default(),
                );
                push(&mut tokens, Kind::Word(unescaped), start..index);
            }
            b'.' => {
                index += 1;
                push(&mut tokens, Kind::Dot, start..index);
            }
            b',' => {
                index += 1;
                push(&mut tokens, Kind::Comma, start..index);
            }
            b'(' => {
                index += 1;
                push(&mut tokens, Kind::Open, start..index);
            }
            b')' => {
                index += 1;
                push(&mut tokens, Kind::Close, start..index);
            }
            byte if byte.is_ascii_whitespace() => index += 1,
            _ => {
                let rest = text.get(index..).unwrap_or_default();
                let length: usize = rest
                    .chars()
                    .take_while(|character| is_identifier_char(*character))
                    .map(char::len_utf8)
                    .sum();
                if length == 0 {
                    index += rest.chars().next().map_or(1, char::len_utf8);
                    push(&mut tokens, Kind::Other, start..index);
                } else {
                    index += length;
                    let word = text.get(start..index).unwrap_or_default().to_owned();
                    push(&mut tokens, Kind::Word(word), start..index);
                }
            }
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at_end(text: &str) -> CompletionContext {
        context_at(text, text.len())
    }

    #[test]
    fn comments_and_multibyte_text_do_not_confuse_the_context() {
        let text = "select /* a /* nested */ from x */ c. from café c";
        let cursor = text.find("c. ").unwrap() + 2;
        let context = context_at(text, cursor);
        assert_eq!(
            context.expectation,
            Expectation::Column {
                qualifier: Some("c".to_owned())
            }
        );
        assert_eq!(context.tables.len(), 1);
        assert_eq!(context.tables[0].name, "café");
        // A cursor inside `é` is not on a character boundary.
        let inside = text.find('é').unwrap() + 1;
        assert_eq!(context_at(text, inside).tables.len(), 1);
    }

    #[test]
    fn columns_of_an_alias_after_a_dot() {
        let text = "select c. from customers c join sales.orders o on o.cid = c.id";
        let context = context_at(text, "select c.".len());
        assert_eq!(
            context.expectation,
            Expectation::Column {
                qualifier: Some("c".to_owned())
            }
        );
        assert_eq!(context.prefix, "");
        assert_eq!(
            context.tables,
            vec![
                TableRef {
                    schema: None,
                    name: "customers".to_owned(),
                    alias: Some("c".to_owned()),
                },
                TableRef {
                    schema: Some("sales".to_owned()),
                    name: "orders".to_owned(),
                    alias: Some("o".to_owned()),
                },
            ]
        );
    }

    #[test]
    fn tables_after_from_and_join() {
        let context = at_end("select * from cus");
        assert_eq!(context.expectation, Expectation::Table { schema: None });
        assert_eq!(context.prefix, "cus");
        assert_eq!(context.replace, 14..17);
        assert_eq!(
            at_end("select * from a join [dbo].").expectation,
            Expectation::Table {
                schema: Some("dbo".to_owned())
            }
        );
        assert_eq!(
            at_end("select * from a x ").expectation,
            Expectation::Keyword
        );
        assert_eq!(
            at_end("select * from a, ").expectation,
            Expectation::Table { schema: None }
        );
    }

    #[test]
    fn columns_in_where_and_select() {
        assert_eq!(
            at_end("select * from t where na").expectation,
            Expectation::Column { qualifier: None }
        );
        assert_eq!(
            at_end("select na").expectation,
            Expectation::Column { qualifier: None }
        );
        assert_eq!(
            at_end("select (select 1 from x) from t order by ").expectation,
            Expectation::Column { qualifier: None }
        );
    }

    #[test]
    fn scope_is_the_current_statement() {
        let text = "select * from a x;\nselect x.";
        let context = at_end(text);
        assert!(context.tables.is_empty(), "{:?}", context.tables);
    }

    #[test]
    fn quoted_names_and_comma_lists() {
        let tables = tables_at(
            r#"select * from "My Table" t, [dbo].[x y] as z where 1=1"#,
            0,
        );
        assert_eq!(tables[0].name, "My Table");
        assert_eq!(tables[0].alias.as_deref(), Some("t"));
        assert_eq!(tables[1].schema.as_deref(), Some("dbo"));
        assert_eq!(tables[1].name, "x y");
        assert_eq!(tables[1].alias.as_deref(), Some("z"));
    }

    #[test]
    fn a_typed_quote_is_part_of_the_replaced_text() {
        let text = "select * from [ord]";
        let context = context_at(text, "select * from [ord".len());
        assert_eq!(context.prefix, "ord");
        assert_eq!(&text[context.replace], "[ord]");
        assert_eq!(
            context_at("select * from [dbo].[", 21).expectation,
            Expectation::Table {
                schema: Some("dbo".to_owned())
            }
        );
        let tables = tables_at("select * from [cus", 0);
        assert_eq!(tables[0].name, "cus");
        let tables = tables_at("select * from [dbé", 0);
        assert_eq!(tables[0].name, "dbé");
    }

    #[test]
    fn names_under_the_cursor() {
        let text = "select c.name from dbo.customers c";
        let (parts, range) = name_at(text, 10).unwrap();
        assert_eq!(parts, vec!["c", "name"]);
        assert_eq!(&text[range], "name");
        let (parts, _) = name_at(text, 26).unwrap();
        assert_eq!(parts, vec!["dbo", "customers"]);
    }
}
