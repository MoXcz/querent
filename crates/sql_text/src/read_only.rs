//! Whether a statement only reads, so a driver may stop it at the row limit.

use crate::completion::{Kind, Token, tokenize};

/// Whether `text` is one statement that only reads, so a client may stop it
/// once it has enough rows. Errs toward `false`: a write word, a word that
/// starts another statement, a second `select` that no set operator joins, or
/// a call to a function that is not a known built-in (it might write)
/// disqualifies it.
#[must_use]
pub fn is_read_only_query(text: &str) -> bool {
    const READS: [&str; 4] = ["select", "with", "values", "table"];
    // A blank line inside a statement is not a second statement to the server.
    if crate::split(text, crate::Delimiters::Semicolon).len() != 1 {
        return false;
    }
    let tokens = tokenize(text, 0);
    let mut words = tokens.iter().filter_map(Token::word);
    let reads = words.next().is_some_and(|first| is_one_of(first, &READS))
        && !words.any(|word| is_one_of(word, NOT_READS));
    let unknown_call = tokens.windows(2).any(|pair| match pair {
        [word, open] if open.kind == Kind::Open => word
            .word()
            .is_some_and(|word| !is_one_of(word, NOT_CALLS) && !is_one_of(word, BUILT_INS)),
        _ => false,
    });
    reads && !unknown_call && !has_second_select(&tokens)
}

fn is_one_of(word: &str, list: &[&str]) -> bool {
    list.iter().any(|item| word.eq_ignore_ascii_case(item))
}

/// T-SQL needs no `;` between statements, so `select 1 select 2` is a batch of
/// two: more than one `select` outside parentheses that no set operator joins.
fn has_second_select(tokens: &[Token]) -> bool {
    const JOINS_SELECTS: [&str; 6] = ["union", "all", "distinct", "intersect", "except", "minus"];
    let leads_with_select = tokens
        .first()
        .is_some_and(|token| token.is_keyword("select") || token.is_keyword("with"));
    let mut depth = 0_usize;
    let mut previous: Option<&Token> = None;
    let mut selects = 0_usize;
    for token in tokens {
        match &token.kind {
            Kind::Open => depth += 1,
            Kind::Close => depth = depth.saturating_sub(1),
            Kind::Word(_) | Kind::Dot | Kind::Comma | Kind::Other => {}
        }
        let joined = previous
            .and_then(Token::word)
            .is_some_and(|word| is_one_of(word, &JOINS_SELECTS));
        if depth == 0 && token.is_keyword("select") && !joined {
            selects += 1;
        }
        previous = Some(token);
    }
    selects > usize::from(leads_with_select)
}

/// Words that write, or that start a statement of their own after a `select`.
const NOT_READS: &[&str] = &[
    "insert",
    "update",
    "delete",
    "merge",
    "into",
    "exec",
    "execute",
    "call",
    "create",
    "alter",
    "drop",
    "truncate",
    "grant",
    "revoke",
    "deny",
    "kill",
    "use",
    "set",
    "declare",
    "dbcc",
    "backup",
    "restore",
    "shutdown",
    "reconfigure",
    "checkpoint",
    "begin",
    "commit",
    "rollback",
    "print",
    "raiserror",
    "throw",
    "waitfor",
    "if",
    "while",
    "go",
    "copy",
    "vacuum",
    "refresh",
    "lock",
    "do",
    "notify",
    "listen",
    "attach",
    "detach",
    "pragma",
    "reindex",
];

/// Words a `(` can follow without being a function call.
const NOT_CALLS: &[&str] = &[
    "select",
    "from",
    "join",
    "on",
    "where",
    "and",
    "or",
    "not",
    "in",
    "exists",
    "as",
    "over",
    "filter",
    "within",
    "any",
    "all",
    "some",
    "using",
    "when",
    "then",
    "else",
    "case",
    "by",
    "having",
    "top",
    "values",
    "table",
    "lateral",
    "union",
    "intersect",
    "except",
    "with",
    "between",
    "is",
    "like",
    "ilike",
    "partition",
    "rows",
    "range",
];

/// Common read-only functions; any other call stops the early cancel.
const BUILT_INS: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "cast",
    "convert",
    "try_cast",
    "try_convert",
    "coalesce",
    "isnull",
    "nullif",
    "iif",
    "ifnull",
    "nvl",
    "upper",
    "lower",
    "trim",
    "ltrim",
    "rtrim",
    "substring",
    "substr",
    "left",
    "right",
    "len",
    "length",
    "char_length",
    "concat",
    "concat_ws",
    "replace",
    "round",
    "floor",
    "ceiling",
    "ceil",
    "abs",
    "power",
    "sqrt",
    "format",
    "year",
    "month",
    "day",
    "datepart",
    "datename",
    "dateadd",
    "datediff",
    "date_trunc",
    "date_part",
    "extract",
    "getdate",
    "getutcdate",
    "sysdatetime",
    "now",
    "current_date",
    "to_char",
    "to_date",
    "to_timestamp",
    "string_agg",
    "array_agg",
    "listagg",
    "row_number",
    "rank",
    "dense_rank",
    "ntile",
    "lag",
    "lead",
    "first_value",
    "last_value",
    "generate_series",
    "unnest",
    "json_value",
    "json_query",
    "openjson",
    "charindex",
    "position",
    "strpos",
    "patindex",
    "stuff",
    "reverse",
    "md5",
    "random",
    "rand",
    "newid",
    "decimal",
    "numeric",
    "varchar",
    "nvarchar",
    "char",
    "nchar",
    "datetime2",
    "float",
    "int",
    "bigint",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_queries() {
        assert!(is_read_only_query("select * from t where a = 'update'"));
        assert!(is_read_only_query(
            "-- top\nwith x as (select 1) select * from x;"
        ));
        assert!(!is_read_only_query("select * into t2 from t"));
        assert!(!is_read_only_query("with x as (select 1) delete from t"));
        assert!(!is_read_only_query("select 1; select 2"));
        assert!(!is_read_only_query("exec sp_who"));
        assert!(!is_read_only_query("select * from t for update"));
        assert!(!is_read_only_query("select log_insert(x) from t"));
        assert!(is_read_only_query(
            "select count(*), cast(a as int) from t where b in (select 1) and exists (select 2)"
        ));
    }

    #[test]
    fn a_batch_without_semicolons_is_not_one_read() {
        assert!(!is_read_only_query("select * from a select * from b"));
        assert!(!is_read_only_query("select * from big kill 53"));
        assert!(!is_read_only_query("select 1 use other"));
        assert!(!is_read_only_query(
            "with x as (select 1) select * from x select 2"
        ));
        assert!(is_read_only_query("select 1 union all select 2"));
        assert!(is_read_only_query(
            "select a from t\n\nwhere a in (select b from u) except select 3"
        ));
        assert!(is_read_only_query(
            "select * from t order by a offset 0 rows fetch next 5 rows only"
        ));
    }
}
