//! SQL admission checks.
//!
//! Two questions this module answers, both deliberately conservative: is this
//! exactly one read statement, and which table does this write touch?
//!
//! Neither is a SQL parser. A parser that agrees with Dolt's own grammar in
//! every case is a large dependency and a large surface; what is needed here is
//! a gate that is *never wrong in the permissive direction*, and may refuse
//! things a full parser would allow. A refused legitimate query is an operator
//! inconvenience; an admitted write on the read path is a breach.

/// Why a statement was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SqlError {
    /// The query was empty.
    #[error("the query is empty")]
    Empty,
    /// More than one statement was supplied.
    #[error(
        "the query contains more than one statement; `dolt sql -q` executes all \
         of them, so `select 1; delete from t` would run the delete — supply \
         exactly one statement"
    )]
    MultipleStatements,
    /// The statement was not a read.
    #[error("`{verb}` is not a read; use the write tool for statements that change data")]
    NotAReadStatement {
        /// The leading keyword found.
        verb: String,
    },
    /// A write named a table outside the allowlist.
    #[error(
        "table `{table}` is not in this integration's writable set ({allowed}); \
         declare it as `capabilities = [\"table:{table}\"]` to permit it"
    )]
    TableNotAllowed {
        /// The table the statement targets.
        table: String,
        /// The tables that are permitted.
        allowed: String,
    },
    /// A write names more than one table.
    #[error(
        "multi-table writes are not permitted: `{0}` forms can modify a table \
         that the FROM clause never names (`delete secrets from notes join \
         secrets ...` empties `secrets`), so this adapter refuses them rather \
         than resolving joins — rewrite it as a single-table statement",
        "JOIN/multi-target"
    )]
    MultiTableWrite,

    /// A write's target table could not be identified.
    #[error(
        "cannot determine which table `{verb}` writes to, so it cannot be \
         checked against the allowlist; use an explicit \
         `INSERT INTO <table>`, `UPDATE <table>` or `DELETE FROM <table>`"
    )]
    UnknownTarget {
        /// The leading keyword found.
        verb: String,
    },
}

/// Statements that only read.
///
/// `WITH` is present because a CTE is normally a read — but `WITH ... INSERT`
/// is accepted by Dolt (verified against a real database: `with c as (select 1)
/// insert into notes values ('x')` adds a row). So leading with `WITH` is not
/// sufficient, and [`ensure_single_read_statement`] additionally scans a `WITH`
/// statement for a mutating keyword.
const READ_VERBS: &[&str] = &["select", "show", "describe", "desc", "explain", "with"];

/// Keywords that mutate data or schema, in any position.
///
/// Used to disqualify a `WITH` statement whose CTE is only a preamble to a
/// write. Matching these anywhere is deliberately blunt: a column literally
/// named `insert` would have to be backtick-quoted to parse in the first place,
/// and refusing an odd-but-legal query is the acceptable direction to be wrong.
const MUTATING_KEYWORDS: &[&str] = &[
    "insert", "update", "delete", "replace", "drop", "alter", "truncate", "create", "rename",
    "grant", "revoke", "call",
];

/// Strip SQL comments so they cannot hide a statement separator.
///
/// `select 1 -- ;drop` is one statement; `select 1 /* ; */ ; drop table t` is
/// two. Counting separators without removing comments gets both wrong, in
/// opposite directions.
///
/// Backslash escapes are honoured inside `'` and `"` strings because Dolt
/// honours them: after `\'` the string is still open. A checker that toggles on
/// every quote ends up in the opposite state to Dolt, and then reads a real
/// separator as string data.
fn strip_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let bytes: Vec<char> = sql.chars().collect();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;

    while i < bytes.len() {
        let c = bytes[i];
        let next = bytes.get(i + 1).copied();

        // Inside a quoted string, a backslash consumes the next character —
        // including a quote, and including another backslash.
        if (in_single || in_double) && c == '\\' {
            out.push(c);
            if let Some(escaped) = next {
                out.push(escaped);
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        if !in_single && !in_double && !in_backtick {
            // `-- ...` to end of line.
            if c == '-' && next == Some('-') {
                while i < bytes.len() && bytes[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            // `# ...` to end of line (MySQL-style, which Dolt accepts).
            if c == '#' {
                while i < bytes.len() && bytes[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            // `/* ... */`, possibly spanning lines.
            if c == '/' && next == Some('*') {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == '*' && bytes[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                // A comment separates tokens, so it becomes whitespace rather
                // than vanishing: `select/**/1` must not become `select1`.
                out.push(' ');
                continue;
            }
        }

        // Track quoting so a `;` or comment marker inside a literal is data.
        match c {
            '\'' if !in_double && !in_backtick => in_single = !in_single,
            '"' if !in_single && !in_backtick => in_double = !in_double,
            '`' if !in_single && !in_double => in_backtick = !in_backtick,
            _ => {}
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Split on statement separators that are not inside a quoted literal.
///
/// Backslash escapes are honoured for the same reason as in [`strip_comments`].
fn split_statements(sql: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    let mut chars = sql.chars().peekable();

    while let Some(c) = chars.next() {
        if (in_single || in_double) && c == '\\' {
            current.push(c);
            if let Some(escaped) = chars.next() {
                current.push(escaped);
            }
            continue;
        }
        match c {
            '\'' if !in_double && !in_backtick => {
                in_single = !in_single;
                current.push(c);
            }
            '"' if !in_single && !in_backtick => {
                in_double = !in_double;
                current.push(c);
            }
            '`' if !in_single && !in_double => {
                in_backtick = !in_backtick;
                current.push(c);
            }
            ';' if !in_single && !in_double && !in_backtick => {
                parts.push(current.trim().to_string());
                current = String::new();
            }
            _ => current.push(c),
        }
    }
    parts.push(current.trim().to_string());
    parts.retain(|p| !p.is_empty());
    parts
}

/// The leading keyword of a statement, lower-cased.
fn leading_verb(statement: &str) -> String {
    statement
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_start_matches('(')
        .to_ascii_lowercase()
}

/// Confirm `sql` is exactly one read statement.
///
/// # Errors
///
/// Returns [`SqlError`] if the query is empty, contains more than one
/// statement, or leads with a verb that is not a read.
pub fn ensure_single_read_statement(sql: &str) -> Result<(), SqlError> {
    let cleaned = strip_comments(sql);
    let statements = split_statements(&cleaned);

    match statements.len() {
        0 => return Err(SqlError::Empty),
        1 => {}
        // The finding that shapes this module: `dolt sql -q` runs every
        // statement, so refusing the whole input is the only safe answer.
        // Classifying each one and admitting "all reads" would still leave the
        // classifier as the single point of failure.
        _ => return Err(SqlError::MultipleStatements),
    }

    let verb = leading_verb(&statements[0]);
    if !READ_VERBS.contains(&verb.as_str()) {
        return Err(SqlError::NotAReadStatement { verb });
    }

    // `WITH` needs a second look: Dolt accepts `with c as (select 1) insert
    // into t values (...)`, which leads with a read keyword and writes. Scan
    // the statement outside string literals for a mutating keyword.
    if verb == "with" {
        let scrubbed = blank_string_literals(&statements[0]);
        for token in scrubbed.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
            let token = token.to_ascii_lowercase();
            if MUTATING_KEYWORDS.contains(&token.as_str()) {
                return Err(SqlError::NotAReadStatement { verb: token });
            }
        }
    }
    Ok(())
}

/// Replace the contents of string literals with spaces.
///
/// Keyword scanning must not fire on `select 'delete'`, where the word is data.
/// Backslash escapes are honoured, as everywhere else in this module.
fn blank_string_literals(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = sql.chars();

    while let Some(c) = chars.next() {
        if (in_single || in_double) && c == '\\' {
            out.push(' ');
            if chars.next().is_some() {
                out.push(' ');
            }
            continue;
        }
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                out.push(' ');
            }
            '"' if !in_single => {
                in_double = !in_double;
                out.push(' ');
            }
            _ if in_single || in_double => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// The table a single write statement targets, checked against `allowed`.
///
/// # Errors
///
/// Returns [`SqlError`] if the query is empty, contains more than one
/// statement, targets a table outside `allowed`, or is a shape whose target
/// cannot be identified.
pub fn table_written_by(sql: &str, allowed: &[String]) -> Result<String, SqlError> {
    let cleaned = strip_comments(sql);
    let statements = split_statements(&cleaned);
    match statements.len() {
        0 => return Err(SqlError::Empty),
        1 => {}
        _ => return Err(SqlError::MultipleStatements),
    }

    let statement = &statements[0];
    // Keyword scanning must not fire on table names inside string literals.
    let scrubbed = blank_string_literals(statement);
    let tokens: Vec<String> = scrubbed
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .collect();
    let verb = tokens.first().cloned().unwrap_or_default();

    // Multi-table write forms are refused outright.
    //
    // Both of these change a table the naive single-target lookup never sees,
    // verified against a real database:
    //
    //   delete secrets from notes join secrets on 1=1   -> empties `secrets`
    //   update notes join secrets on 1=1 set secrets.id='X' -> rewrites `secrets`
    //
    // Resolving every modified table in these shapes means implementing join
    // resolution and alias tracking. Refusing them is the conservative answer:
    // a multi-table write is not something this adapter needs to offer, and the
    // failure mode of getting it wrong is writing to an unlisted table.
    let has_join = tokens.iter().any(|t| {
        matches!(
            t.as_str(),
            "join" | "inner" | "left" | "right" | "cross" | "straight_join" | "natural"
        )
    });
    if has_join {
        return Err(SqlError::MultiTableWrite);
    }

    // Only these three shapes are admitted. DDL (`drop`, `alter`, `truncate`)
    // is deliberately absent: an allowlist of tables cannot meaningfully
    // constrain a statement that removes one, and a schema change is an
    // operator action rather than an agent action.
    let table_token = match verb.as_str() {
        "insert" | "replace" => tokens
            .iter()
            .position(|t| t == "into")
            .and_then(|i| tokens.get(i + 1)),
        "update" => {
            // `update a, b set ...` is a multi-table update written with a
            // comma instead of a JOIN.
            let set_at = tokens.iter().position(|t| t == "set");
            let target = tokens.get(1);
            // `update a, b set ...` — a comma is the other multi-table spelling.
            if target.is_some_and(|t| t.contains(',')) {
                return Err(SqlError::MultiTableWrite);
            }
            match (set_at, target) {
                // Exactly one token may sit between `update` and `set`.
                (Some(2), Some(_)) => target,
                (Some(_), _) => return Err(SqlError::MultiTableWrite),
                (None, _) => return Err(SqlError::UnknownTarget { verb }),
            }
        }
        "delete" => {
            // `delete t1 from ...` names its targets BEFORE `from`. Only the
            // plain `delete from <table>` shape is admitted.
            match tokens.get(1).map(String::as_str) {
                Some("from") => {
                    if tokens.get(2).is_some_and(|t| t.contains(',')) {
                        return Err(SqlError::MultiTableWrite);
                    }
                    tokens.get(2)
                }
                Some(_) => return Err(SqlError::MultiTableWrite),
                None => return Err(SqlError::UnknownTarget { verb }),
            }
        }
        _ => {
            return Err(SqlError::NotAReadStatement { verb });
        }
    };

    let Some(raw) = table_token else {
        return Err(SqlError::UnknownTarget { verb });
    };

    // Strip a trailing `(` from `insert into t(col)` FIRST, then remove
    // quoting: `` `knowledge`(id) `` splits to `` `knowledge` ``, which only
    // trims correctly once the parenthesis is gone. Doing it the other way
    // leaves a stray backtick and the allowlist comparison fails on a table
    // that should have been permitted.
    let table = raw
        .split('(')
        .next()
        .unwrap_or_default()
        .trim_matches(|c| c == '`' || c == '"' || c == '\'')
        // A qualified name `db.table` is checked on its final segment, since
        // the allowlist names tables within the configured clone.
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .trim_matches(|c| c == '`' || c == '"' || c == '\'')
        .to_string();

    if table.is_empty() {
        return Err(SqlError::UnknownTarget { verb });
    }
    if !allowed.iter().any(|a| a.eq_ignore_ascii_case(&table)) {
        return Err(SqlError::TableNotAllowed {
            table,
            allowed: allowed.join(", "),
        });
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_select_is_admitted() {
        for good in [
            "select * from knowledge",
            "  SELECT id FROM t WHERE x = 1  ",
            "select 1;",
            "show tables",
            "explain select 1",
            "with x as (select 1) select * from x",
        ] {
            assert!(
                ensure_single_read_statement(good).is_ok(),
                "`{good}` is a single read and must be admitted"
            );
        }
    }

    #[test]
    fn stacked_statements_are_refused_even_when_the_first_is_a_read() {
        // The property this module exists for, verified experimentally against
        // dolt: `dolt sql -q "select 1; insert into t values (99)"` returns the
        // select AND persists the insert. A prefix check admits it.
        for attack in [
            "select 1; insert into knowledge values ('x')",
            "select 1; delete from knowledge",
            "select 1; drop table knowledge",
            "select 1 ; update knowledge set title = 'x'",
            "SELECT 1;\nDELETE FROM knowledge",
        ] {
            assert_eq!(
                ensure_single_read_statement(attack),
                Err(SqlError::MultipleStatements),
                "`{attack}` stacks a write behind a read and must be refused"
            );
        }
    }

    #[test]
    fn a_semicolon_inside_a_literal_is_data_not_a_separator() {
        // Refusing this would be the over-blocking failure: the query is one
        // statement and the `;` is part of a string.
        assert!(
            ensure_single_read_statement("select * from t where s = 'a;b'").is_ok(),
            "a `;` inside a quoted literal does not separate statements"
        );
        assert!(ensure_single_read_statement("select `we;ird` from t").is_ok());
    }

    #[test]
    fn a_comment_cannot_hide_or_invent_a_separator() {
        // `--` comments out the rest of the line, so this is one statement.
        assert!(ensure_single_read_statement("select 1 -- ; delete from t").is_ok());
        // A block comment containing `;` is not a separator either.
        assert!(ensure_single_read_statement("select 1 /* ; */ from t").is_ok());
        // But a real separator after a comment still counts.
        assert_eq!(
            ensure_single_read_statement("select 1 /* c */ ; delete from t"),
            Err(SqlError::MultipleStatements)
        );
    }

    #[test]
    fn a_write_on_the_read_path_is_refused() {
        for write in [
            "insert into t values (1)",
            "delete from t",
            "update t set x = 1",
            "drop table t",
            "create table t (id int)",
        ] {
            assert!(
                matches!(
                    ensure_single_read_statement(write),
                    Err(SqlError::NotAReadStatement { .. })
                ),
                "`{write}` is not a read"
            );
        }
    }

    #[test]
    fn a_write_must_name_an_allowed_table() {
        let allowed = vec!["knowledge".to_string()];

        assert_eq!(
            table_written_by("insert into knowledge values ('a')", &allowed).expect("allowed"),
            "knowledge"
        );
        assert_eq!(
            table_written_by("INSERT INTO `knowledge`(id) VALUES ('a')", &allowed)
                .expect("quoting and a column list do not change the table"),
            "knowledge"
        );
        assert_eq!(
            table_written_by("update knowledge set title = 'x'", &allowed).expect("allowed"),
            "knowledge"
        );
        assert_eq!(
            table_written_by("delete from knowledge where id = '1'", &allowed).expect("allowed"),
            "knowledge"
        );

        assert!(matches!(
            table_written_by("insert into secrets values ('a')", &allowed),
            Err(SqlError::TableNotAllowed { .. })
        ));
        assert!(matches!(
            table_written_by("delete from secrets", &allowed),
            Err(SqlError::TableNotAllowed { .. })
        ));
    }

    #[test]
    fn ddl_is_refused_on_the_write_path_too() {
        // A table allowlist cannot constrain a statement that drops a table, so
        // schema changes stay an operator action.
        let allowed = vec!["knowledge".to_string()];
        for ddl in [
            "drop table knowledge",
            "alter table knowledge add column x int",
            "truncate table knowledge",
            "create table other (id int)",
        ] {
            assert!(
                table_written_by(ddl, &allowed).is_err(),
                "`{ddl}` is DDL and must not pass the write gate"
            );
        }
    }

    #[test]
    fn stacked_statements_are_refused_on_the_write_path_as_well() {
        let allowed = vec!["knowledge".to_string()];
        assert_eq!(
            table_written_by(
                "insert into knowledge values ('a'); delete from secrets",
                &allowed
            ),
            Err(SqlError::MultipleStatements),
            "an allowed first statement must not carry a second one along"
        );
    }

    #[test]
    fn a_qualified_table_name_is_checked_on_its_final_segment() {
        let allowed = vec!["knowledge".to_string()];
        assert_eq!(
            table_written_by("insert into mydb.knowledge values ('a')", &allowed).expect("allowed"),
            "knowledge"
        );
        assert!(matches!(
            table_written_by("insert into mydb.secrets values ('a')", &allowed),
            Err(SqlError::TableNotAllowed { .. })
        ));
    }

    #[test]
    fn an_empty_query_is_refused_on_both_paths() {
        assert_eq!(ensure_single_read_statement("   "), Err(SqlError::Empty));
        assert_eq!(table_written_by(" ; ", &[]), Err(SqlError::Empty));
    }
}

#[cfg(test)]
mod escape_tests {
    use super::*;

    /// A backslash-escaped quote must not desynchronise quote tracking.
    ///
    /// Dolt accepts `\'` as a literal quote inside a string (verified against a
    /// real database). Tracking quotes by toggling on every `'` therefore ends
    /// up in the OPPOSITE state to Dolt after `\'`, and one more quote puts the
    /// checker "inside a string" while Dolt is outside — at which point a real
    /// statement separator is read as data and the statement behind it is
    /// invisible.
    ///
    /// Proven against dolt before this test was written:
    ///
    /// ```text
    /// dolt sql -q "select 'a\'' ; insert into notes values ('hacked')"
    /// -> row count 1 -> 2; the insert ran
    /// ```
    #[test]
    fn a_backslash_escaped_quote_cannot_hide_a_statement_separator() {
        let attack = r"select 'a\'' ; insert into notes values ('hacked')";
        assert_eq!(
            ensure_single_read_statement(attack),
            Err(SqlError::MultipleStatements),
            "dolt runs the insert in this input, so it must never be admitted"
        );
    }

    /// The same desynchronisation on the write path.
    #[test]
    fn a_backslash_escaped_quote_cannot_hide_a_second_write() {
        let allowed = vec!["knowledge".to_string()];
        let attack = r"insert into knowledge values ('a\'') ; delete from secrets";
        assert_eq!(
            table_written_by(attack, &allowed),
            Err(SqlError::MultipleStatements),
            "an allowed first statement must not smuggle a second one"
        );
    }

    /// An escaped backslash does NOT escape the quote that follows it.
    ///
    /// `'a\\'` is a complete string containing one backslash, so the `;` after
    /// it is a real separator.
    #[test]
    fn an_escaped_backslash_does_not_escape_the_closing_quote() {
        let attack = r"select 'a\\' ; delete from notes";
        assert_eq!(
            ensure_single_read_statement(attack),
            Err(SqlError::MultipleStatements)
        );
    }

    /// Restrictive-direction check: a legitimate escaped quote in a single
    /// statement still parses as one statement and is admitted.
    #[test]
    fn an_escaped_quote_in_a_legitimate_single_statement_is_still_admitted() {
        assert!(
            ensure_single_read_statement(r"select * from t where s = 'it\'s fine'").is_ok(),
            "a lone escaped quote inside one statement is legitimate"
        );
        assert!(ensure_single_read_statement(r"select * from t where s = 'back\\slash'").is_ok());
    }
}

#[cfg(test)]
mod cte_tests {
    use super::*;

    /// `WITH` leads with a read keyword but can carry a write.
    ///
    /// Verified against a real dolt database before this test existed:
    /// `with c as (select 1) insert into notes values ('x')` raised the row
    /// count. Leading-verb classification alone therefore admits a write.
    #[test]
    fn a_cte_preamble_cannot_carry_a_write_past_the_read_gate() {
        for attack in [
            "with c as (select 1) insert into notes values ('x')",
            "WITH c AS (SELECT 1) DELETE FROM notes",
            "with c as (select 1) update notes set id = 'z'",
            "with c as (select 1) replace into notes values ('x')",
        ] {
            assert!(
                matches!(
                    ensure_single_read_statement(attack),
                    Err(SqlError::NotAReadStatement { .. })
                ),
                "`{attack}` writes despite leading with WITH and must be refused"
            );
        }
    }

    /// Restrictive direction: a genuine read CTE is still admitted.
    #[test]
    fn a_read_only_cte_is_still_admitted() {
        for good in [
            "with c as (select 1) select * from c",
            "WITH recent AS (SELECT id FROM notes) SELECT count(*) FROM recent",
        ] {
            assert!(
                ensure_single_read_statement(good).is_ok(),
                "`{good}` is a read and must be admitted"
            );
        }
    }

    /// A mutating word inside a string literal is data, not a statement.
    #[test]
    fn a_keyword_inside_a_literal_does_not_disqualify_a_read() {
        assert!(
            ensure_single_read_statement("with c as (select 'delete') select * from c").is_ok(),
            "the word `delete` here is a string value, not a verb"
        );
    }
}

#[cfg(test)]
mod multi_table_tests {
    use super::*;

    /// A multi-table DELETE names its target BEFORE `from`.
    ///
    /// Verified against a real database: with only `notes` allowlisted,
    /// `delete secrets from notes join secrets on 1=1` emptied `secrets`
    /// (row count 1 -> 0). Looking for the table after `from` finds `notes`,
    /// which is allowed — so the naive lookup admits a write to an unlisted
    /// table.
    #[test]
    fn a_multi_table_delete_cannot_reach_an_unlisted_table() {
        let allowed = vec!["notes".to_string()];
        for attack in [
            "delete secrets from notes join secrets on 1=1",
            "DELETE secrets FROM notes INNER JOIN secrets ON 1=1",
            "delete notes, secrets from notes join secrets on 1=1",
        ] {
            let err = table_written_by(attack, &allowed)
                .expect_err("a multi-table delete must be refused");
            assert_eq!(
                err,
                SqlError::MultiTableWrite,
                "`{attack}` deletes from an unlisted table"
            );
        }
    }

    /// A multi-table UPDATE can assign to a table the allowlist never sees.
    ///
    /// Also verified against a real database: with only `notes` allowlisted,
    /// `update notes join secrets on 1=1 set secrets.id = 'X'` rewrote a row in
    /// `secrets`.
    #[test]
    fn a_multi_table_update_cannot_reach_an_unlisted_table() {
        let allowed = vec!["notes".to_string()];
        for attack in [
            "update notes join secrets on 1=1 set secrets.id = 'X'",
            "UPDATE notes LEFT JOIN secrets ON 1=1 SET secrets.id = 'X'",
            "update notes, secrets set secrets.id = 'X'",
        ] {
            let err = table_written_by(attack, &allowed)
                .expect_err("a multi-table update must be refused");
            assert_eq!(
                err,
                SqlError::MultiTableWrite,
                "`{attack}` writes to an unlisted table"
            );
        }
    }

    /// Restrictive direction: ordinary single-table writes still work.
    #[test]
    fn single_table_writes_are_unaffected() {
        let allowed = vec!["notes".to_string()];
        for good in [
            "insert into notes values ('a')",
            "update notes set id = 'b' where id = 'a'",
            "delete from notes where id = 'a'",
        ] {
            assert_eq!(
                table_written_by(good, &allowed).expect("a single-table write is allowed"),
                "notes",
                "`{good}` must still be admitted"
            );
        }
    }

    /// A table name inside a literal must not be mistaken for a target.
    #[test]
    fn a_join_keyword_inside_a_literal_does_not_refuse_a_legitimate_write() {
        let allowed = vec!["notes".to_string()];
        assert_eq!(
            table_written_by("insert into notes values ('join')", &allowed)
                .expect("the word `join` here is data"),
            "notes"
        );
    }
}
