//! SQL admission checks.
//!
//! Two questions this module answers, both deliberately conservative: is this
//! exactly one read statement, and which table does this write touch?
//!
//! The gate is a strict-subset parse boundary (gh#469, review round 4): a
//! statement is admitted only when a real SQL parser (MySQL dialect) parses
//! the input into exactly one statement of a whitelisted shape whose resolved
//! table identifiers are checked against the allowlist WITHOUT lexical
//! blanking or quote-stripping. Lexical facts the parser cannot see are
//! enforced before parsing: executable `/*! ... */` comments (dolt expands and
//! runs their bodies while a parser discards them), `#` comments (dolt's
//! statement splitter splits on `;` inside them while a parser does not —
//! review round 5), and any control or non-ASCII character (dolt ends `#`
//! comments at codepoints a parser treats as comment text). After parsing,
//! every nested query in the statement — CTE bodies at any depth,
//! parenthesized queries, set operands, derived tables, scalar subqueries,
//! `INSERT ... SELECT` sources, and the WHERE/limit filters of modeled SHOW
//! variants — is walked and the statement is refused if any of them mutates
//! (review rounds 6–8). Unmodeled SHOW variants are refused outright (the
//! parser's fallback for them swallows `;`), and a token-level guard refuses
//! any single parsed statement containing a non-trailing `;` so a swallowed
//! statement boundary fails closed (round 8, live-verified).
//!
//! Anything that does not parse is refused — fail-closed. A refused
//! legitimate query is an operator inconvenience; an admitted write on the
//! read path is a breach.

use std::ops::ControlFlow;

use sqlparser::ast::Visit;
use sqlparser::ast::{
    Delete, FromTable, ObjectName, SetExpr, Statement, TableFactor, TableObject,
    UpdateTableFromKind,
};
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::Parser;

/// Why a statement was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SqlError {
    /// The query was empty.
    #[error("the query is empty")]
    Empty,
    /// More than one statement was supplied — counted on the parsed
    /// statements OR on `;` tokens the parse consumed (a parser that
    /// swallows a separator still leaves the token in the stream; round 8).
    #[error(
        "the query contains more than one statement; `dolt sql -q` executes all \
         of them, so `select 1; delete from t` would run the delete — supply \
         exactly one statement"
    )]
    MultipleStatements,
    /// The query uses MySQL conditional-execution comment syntax.
    #[error(
        "executable comments (`/*! ... */`) are not accepted: Dolt expands and \
         runs their bodies, and their contents cannot be scanned reliably — \
         rewrite the statement without version-conditional syntax"
    )]
    ExecutableComment,
    /// The input contains a control character that ends comments in dolt
    /// while a standard parser treats it as ordinary whitespace.
    #[error(
        "control characters are not accepted: dolt ends `#` comments at a \
         carriage return (`--` comments end only at a newline), so a comment \
         could hide a real statement separator — rewrite the statement on a \
         single line"
    )]
    ControlCharacter,
    /// The input uses MySQL `#` comments.
    #[error(
        "`#` comments are not accepted: dolt's statement splitter splits on \
         `;` inside them while a standard parser does not, so a comment can \
         hide a real statement separator — use `--` or `/* */` comments"
    )]
    HashComment,
    /// The input contains a non-ASCII character.
    #[error(
        "non-ASCII characters are not accepted: dolt ends `#` comments at any \
         non-ASCII codepoint while a parser treats it as comment text, so a \
         comment can hide a real statement separator — rewrite the statement \
         in ASCII"
    )]
    NonAsciiCharacter,
    /// The statement did not parse.
    #[error(
        "the statement does not parse as a single MySQL-dialect statement, so \
         it cannot be classified safely; rewrite it as plain SELECT/INSERT/\
         UPDATE/DELETE"
    )]
    NotParseable,
    /// The statement was not a read.
    #[error("`{verb}` is not a read; use the write tool for statements that change data")]
    NotAReadStatement {
        /// The leading keyword found.
        verb: String,
    },
    /// A nested query inside the statement mutates (any depth, any shape).
    #[error(
        "a nested query in this statement mutates data; nested mutations are not admitted on either path"
    )]
    NestedMutation,
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

/// Refuse inputs whose comment termination differs between dolt and the
/// parser, and executable comments outright.
///
/// - `/*! ... */` is MySQL conditional-execution syntax: dolt EXPANDS it and
///   runs the contents, while a standard parser discards it as a comment.
///   Review round 2 proved by live probe that no lexical treatment of the
///   body is sound (quotes inside it poison quote tracking; a version prefix
///   glued to the verb hides the keyword), so it is refused outright.
/// - Other C0 control characters (apart from newline and tab) end `#` and
///   `--` comments in dolt but not in the parser: `SELECT 1# c<CR>; INSERT
///   ...` is TWO statements to dolt and ONE to the parser (review round 4,
///   live-verified: the INSERT persisted through the read tool). Refusing the
///   whole class is the only sound treatment that keeps the two lexers from
///   disagreeing about where a comment ends.
fn lexical_preflight(sql: &str) -> Result<(), SqlError> {
    // All three refusals are QUOTE-UNAWARE by design: a `#` inside a string
    // literal ('a # b') or a backtick identifier (`a#b`) is refused too. That
    // is over-refusal in the safe direction — the alternative is a quote
    // tracker that can be desynchronized from dolt's own lexer (rounds 2–4 of
    // gh#469 were exactly that), and dolt's splitter disagreement applies to
    // the RAW text it lexes, not to the parser's post-quote view of it.
    for c in sql.chars() {
        match c {
            '#' => return Err(SqlError::HashComment),
            '\n' | '\t' => {}
            c if (c as u32) < 0x20 || (c as u32) == 0x7f => {
                return Err(SqlError::ControlCharacter);
            }
            c if (c as u32) > 0x7f => return Err(SqlError::NonAsciiCharacter),
            _ => {}
        }
    }
    // Executable comments are refused separately: a quote-aware scan would be
    // needed to find `/*` inside literals, but refusing `/*!` ANYWHERE is
    // sound for the same reason as `#` — no legitimate statement in the
    // admitted subset needs it.
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            if chars.peek() == Some(&'!') {
                return Err(SqlError::ExecutableComment);
            }
        }
    }
    Ok(())
}

/// Parse `sql` into exactly one statement, or explain why not.
fn parse_one(sql: &str) -> Result<Statement, SqlError> {
    if sql.trim().is_empty() {
        return Err(SqlError::Empty);
    }
    lexical_preflight(sql)?;
    // Swallow guard (review round 8, F8-1): a single PARSED statement whose
    // TOKEN stream contains a `;` before the final token means the parser
    // swallowed a statement boundary the engine will still honor —
    // sqlparser's SHOW fallback (`Statement::ShowVariable`) runs
    // parse_identifiers(), which advances over `;` and everything after it
    // as bare identifiers, so `show engines; delete from secrets` parses as
    // ONE admitted read while dolt executes BOTH (live-verified). Counting
    // semicolons on the token stream (not the raw string) is quote-aware:
    // a `;` inside a string literal is one SingleQuotedString token and
    // never matches; a trailing `;` after a complete statement is allowed.
    let tokens = sqlparser::tokenizer::Tokenizer::new(&MySqlDialect {}, sql)
        .tokenize()
        .map_err(|_| SqlError::NotParseable)?;
    // Ignore whitespace when locating the trailing token: `select 1; ` must
    // stay a single statement (the parser treats a trailing `;` + whitespace
    // as the statement's end, not a second empty statement).
    let significant: Vec<&sqlparser::tokenizer::Token> = tokens
        .iter()
        .filter(|t| !matches!(t, sqlparser::tokenizer::Token::Whitespace(_)))
        .collect();
    let semis = significant
        .iter()
        .filter(|t| matches!(t, sqlparser::tokenizer::Token::SemiColon))
        .count();
    if semis > 1 {
        return Err(SqlError::MultipleStatements);
    }
    if semis == 1 && significant.last() != Some(&&sqlparser::tokenizer::Token::SemiColon) {
        return Err(SqlError::MultipleStatements);
    }
    let statements =
        Parser::parse_sql(&MySqlDialect {}, sql).map_err(|_| SqlError::NotParseable)?;
    match statements.len() {
        0 => Err(SqlError::Empty),
        1 => Ok(statements.into_iter().next().expect("len checked")),
        // `dolt sql -q` runs every statement, so refusing the whole input is
        // the only safe answer — classifying each one and admitting "all
        // reads" would leave the classifier as a single point of failure.
        _ => Err(SqlError::MultipleStatements),
    }
}

/// The final (table) segment of an [`ObjectName`], preserving the
/// identifier's exact value.
///
/// Quoted segments carry their precise name (`'notes'` stays `'notes'`,
/// never `notes`); the qualifier is dropped because the allowlist names
/// tables within the configured clone.
fn table_segment(name: &ObjectName) -> String {
    name.0
        .last()
        .and_then(|part| part.as_ident())
        .map(|ident| ident.value.clone())
        .unwrap_or_default()
}

/// Every table this statement reads or writes, from the AST.
///
/// A write whose FROM/USING/JOIN surfaces name any table beyond the single
/// allowlisted target can modify that table (`delete secrets from notes join
/// secrets ...` empties `secrets`; `delete from notes using secrets as notes`
/// rebinds the target), so the whole class is refused rather than resolved.
fn referenced_tables(statement: &Statement) -> Vec<String> {
    fn push_factor(tables: &mut Vec<String>, factor: &TableFactor) {
        match factor {
            TableFactor::Table { name, .. } => tables.push(table_segment(name)),
            // Derived/nested shapes are not bare tables; an empty marker makes
            // the single-target check refuse the statement.
            _ => tables.push(String::new()),
        }
    }
    fn push_twj(tables: &mut Vec<String>, twj: &sqlparser::ast::TableWithJoins) {
        push_factor(tables, &twj.relation);
        for join in &twj.joins {
            push_factor(tables, &join.relation);
        }
    }
    // Note: tables READ by an INSERT ... SELECT source are deliberately not
    // collected — a read does not write. Only tables the statement can modify
    // (its target list, FROM/USING/JOIN surfaces of DML) are collected.
    let mut tables = Vec::new();
    match statement {
        Statement::Insert(insert) => match &insert.table {
            TableObject::TableName(name) => tables.push(table_segment(name)),
            TableObject::TableFunction(_) => tables.push(String::new()),
        },
        Statement::Update { table, from, .. } => {
            push_twj(&mut tables, table);
            if let Some(from) = from {
                let list = match from {
                    UpdateTableFromKind::BeforeSet(list) | UpdateTableFromKind::AfterSet(list) => {
                        list
                    }
                };
                for twj in list {
                    push_twj(&mut tables, twj);
                }
            }
        }
        Statement::Delete(Delete {
            tables: targets,
            from,
            using,
            ..
        }) => {
            for name in targets {
                tables.push(table_segment(name));
            }
            let list = match from {
                FromTable::WithFromKeyword(list) | FromTable::WithoutKeyword(list) => list,
            };
            for twj in list {
                push_twj(&mut tables, twj);
            }
            if let Some(using) = using {
                for twj in using {
                    push_twj(&mut tables, twj);
                }
            }
        }
        _ => {}
    }
    tables
}

/// Confirm `sql` is exactly one read statement.
///
/// A read is: a plain `SELECT` (optionally under `WITH` clauses whose CTE
/// bodies are themselves pure reads, at any nesting depth), a `SHOW`, a
/// `DESCRIBE`, or an `EXPLAIN` wrapping one of those. `EXPLAIN` wrapping a
/// mutating statement is NOT a read (a version that executes analyzed DML
/// would bypass through the read path); a `WITH ... INSERT` is a write
/// regardless of its read-looking preamble — that is exactly how the parser
/// shapes it (`Query { body: Insert }`), and this check refuses it. Any
/// nested query whose body mutates is likewise refused wherever it sits —
/// CTE bodies, parenthesized query expressions, set-operation operands,
/// derived tables, scalar subqueries, and `INSERT ... SELECT` sources
/// (review rounds 5–6): dolt 2.3.3 rejects DML CTEs today, but an engine
/// that accepts them would turn an admitted "read" into a write.
///
/// # Errors
///
/// Returns [`SqlError`] if the query is empty, contains control characters or
/// executable comments, does not parse, contains more than one statement, or
/// is not a read.
pub fn ensure_single_read_statement(sql: &str) -> Result<(), SqlError> {
    let statement = parse_one(sql)?;
    if is_pure_read(&statement) {
        Ok(())
    } else {
        Err(SqlError::NotAReadStatement {
            verb: statement_verb(&statement),
        })
    }
}

/// The leading verb of a parsed statement, for error messages.
fn statement_verb(statement: &Statement) -> String {
    let text = statement.to_string();
    text.split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_start_matches('(')
        .to_ascii_lowercase()
}

/// A read and nothing else.
///
/// The purity walk runs on the STATEMENT before the shape match: SHOW
/// statements carry their subqueries in WHERE filters (`ShowStatementFilter::
/// Where(Expr)`), which the shape arms below never see (review round 7,
/// F7-1). Walking every Query node first gates every admitted shape —
/// SELECT, SHOW, DESCRIBE, EXPLAIN — against nested mutation uniformly.
fn is_pure_read(statement: &Statement) -> bool {
    let mut purity = QueryPurityVisitor::default();
    let _ = statement.visit(&mut purity);
    if purity.impure {
        return false;
    }
    match statement {
        Statement::Query(query) => {
            // A CTE body that mutates makes the whole statement a write
            // candidate even though the outer body is a SELECT (review
            // round 5, F2), and the mutation can hide in ANY nested query
            // position — a parenthesized query expression, a set-operation
            // operand, a derived table, a scalar subquery, or an INSERT
            // source (review round 6, check 2). The visitor above walks
            // every `Query` node in the parsed statement; what remains here
            // is the shape check on the outer body. dolt 2.3.3 rejects DML
            // CTEs everywhere today (rc=1, live-verified rounds 6–7); this
            // is defense-in-depth for engines that accept them.
            setexpr_is_pure_read(&query.body)
        }
        // `Statement::ShowVariable` — the parser's fallback for every SHOW
        // variant it does not model — is deliberately NOT admitted: its
        // ident list swallows `;` and flattens WHERE subqueries (review
        // round 8, F8-1/F8-2). Unmodeled SHOW variants are refused
        // fail-closed; parse_one additionally refuses any single parsed
        // statement containing a non-trailing `;` token.
        Statement::ShowTables { .. }
        | Statement::ShowColumns { .. }
        | Statement::ShowCreate { .. }
        | Statement::ShowCollation { .. }
        | Statement::ShowFunctions { .. }
        | Statement::ShowVariables { .. }
        | Statement::ShowStatus { .. }
        | Statement::ShowDatabases { .. }
        | Statement::ShowSchemas { .. }
        | Statement::ExplainTable { .. } => true,
        Statement::Explain { statement, .. } => is_pure_read(statement),
        _ => false,
    }
}

/// A query body is a pure read when it is a Select (or set operation / nested
/// query of Selects) — NOT an `Insert`/`Update`/`Delete` body, which is how
/// `WITH c AS (SELECT 1) INSERT INTO t ...` parses.
fn setexpr_is_pure_read(expr: &SetExpr) -> bool {
    match expr {
        SetExpr::Select(select) => {
            // `SELECT ... INTO <table>` names a table destination: it is a
            // write shape, not a read (review round 5, F3). OUTFILE/DUMPFILE
            // forms do not parse in this dialect and are refused upstream.
            select.into.is_none()
        }
        SetExpr::Query(inner) => setexpr_is_pure_read(&inner.body),
        SetExpr::SetOperation { left, right, .. } => {
            setexpr_is_pure_read(left) && setexpr_is_pure_read(right)
        }
        SetExpr::Values(_) => true,
        SetExpr::Insert(_) | SetExpr::Update(_) | SetExpr::Table(_) => false,
    }
}

/// Flags any `Query` node in a statement whose body mutates or whose select
/// carries an INTO destination, wherever it sits (CTE body, parenthesized
/// query, set operand, derived table, scalar subquery, INSERT source).
#[derive(Default)]
struct QueryPurityVisitor {
    impure: bool,
}

impl sqlparser::ast::Visitor for QueryPurityVisitor {
    type Break = ();
    fn pre_visit_query(&mut self, query: &sqlparser::ast::Query) -> ControlFlow<()> {
        let body_impure = match &*query.body {
            sqlparser::ast::SetExpr::Insert(_) | sqlparser::ast::SetExpr::Update(_) => true,
            sqlparser::ast::SetExpr::Select(select) => select.into.is_some(),
            _ => false,
        };
        if body_impure {
            self.impure = true;
        }
        ControlFlow::Continue(())
    }
}

/// The table a single write statement targets, checked against `allowed`.
///
/// Only three shapes are admitted, classified from the parsed AST:
/// `INSERT [IGNORE] [INTO] <table> ...`, `UPDATE <table> SET ...` and
/// `DELETE FROM <table> ...` — with no JOIN, no comma target list, no USING
/// clause, and no second table anywhere the statement touches. DDL is
/// refused: an allowlist of tables cannot meaningfully constrain a statement
/// that removes one.
///
/// # Errors
///
/// Returns [`SqlError`] if the query is empty, contains control characters or
/// executable comments, does not parse, contains more than one statement, is
/// not one of the three admitted write shapes, is a multi-table write, or
/// targets a table outside `allowed`.
pub fn table_written_by(sql: &str, allowed: &[String]) -> Result<String, SqlError> {
    let statement = parse_one(sql)?;
    let verb = statement_verb(&statement);

    if !matches!(
        &statement,
        Statement::Insert(_) | Statement::Update { .. } | Statement::Delete(_)
    ) {
        return Err(SqlError::NotAReadStatement { verb });
    }

    // MySQL `REPLACE [INTO]` parses as Statement::Insert with
    // replace_into = true: it DELETES conflicting rows before inserting — a
    // delete the table allowlist never consented to. The write tool admits
    // plain INSERT only.
    if let Statement::Insert(insert) = &statement {
        if insert.replace_into {
            return Err(SqlError::NotAReadStatement { verb });
        }
    }

    // A DML statement's own source/subqueries can hide a nested mutation
    // (`INSERT INTO notes WITH x AS (INSERT INTO secrets ...) SELECT 1`):
    // the write is to `notes`, but the nested query body writes `secrets`
    // (review round 6, check 2). Walk every Query node in the statement.
    let mut purity = QueryPurityVisitor::default();
    let _ = statement.visit(&mut purity);
    if purity.impure {
        return Err(SqlError::NestedMutation);
    }

    // Exactly one distinct, non-empty table name may be referenced anywhere.
    // This closes the whole laundering family at once: JOIN forms, comma
    // target lists (spaced, glued, or one-sided), USING clauses (alias
    // rebinding included), qualified and quoted qualifiers (`db`.`notes`
    // resolves on its final segment), and quoted names containing quote
    // characters (`'notes'` stays `'notes'`).
    let mut distinct = referenced_tables(&statement);
    distinct.sort();
    distinct.dedup();
    let Some(table) = distinct.first() else {
        return Err(SqlError::UnknownTarget { verb });
    };
    if distinct.len() != 1 || table.is_empty() {
        return Err(SqlError::MultiTableWrite);
    }
    let table = table.clone();

    if !allowed.iter().any(|a| a.eq_ignore_ascii_case(&table)) {
        return Err(SqlError::TableNotAllowed {
            table: table.clone(),
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
                ensure_single_read_statement(attack).is_err(),
                "`{attack}` writes despite leading with WITH and must be refused"
            );
        }
        // Parsed CTE-wrapped DML is classified precisely: Query { body: DML }
        // is not a read. The DELETE/REPLACE spellings fail the parse itself
        // in this dialect, which refuses them equally (verified: both spell
        // ParserError on the parser used by the gate).
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
        // JOIN forms parse and are classified as multi-table writes.
        for attack in [
            "update notes join secrets on 1=1 set secrets.id = 'X'",
            "UPDATE notes LEFT JOIN secrets ON 1=1 SET secrets.id = 'X'",
        ] {
            let err = table_written_by(attack, &allowed)
                .expect_err("a multi-table update must be refused");
            assert_eq!(
                err,
                SqlError::MultiTableWrite,
                "`{attack}` writes to an unlisted table"
            );
        }
        // The comma form (`update a, b set ...`) does not parse in the MySQL
        // dialect, so the parse boundary refuses it as unclassifiable —
        // still refused, fail-closed, just with the parse diagnosis.
        assert!(
            table_written_by("update notes, secrets set secrets.id = 'X'", &allowed).is_err(),
            "the comma-separated multi-table update must be refused"
        );
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

#[cfg(test)]
mod issue_469_tests {
    use super::*;

    /// An executable comment is not a comment to dolt (#469, part 1).
    ///
    /// `/*!50000 ... */` is MySQL's conditional-execution syntax: dolt expands
    /// it and runs its contents. [`strip_comments`] removes it as if it were
    /// inert, so the `WITH` mutation scan below sees a clean read while dolt
    /// executes an INSERT.
    ///
    /// Verified against a real database (dolt 2.3.3, disposable fixture)
    /// before this test was written:
    ///
    /// ```text
    /// dolt sql -q "WITH c AS (SELECT 1) /*!50000 INSERT INTO notes VALUES ('cmt') */"
    /// -> row count 1 -> 2; the insert ran
    /// ```
    #[test]
    fn an_executable_comment_cannot_hide_a_mutation_from_the_read_gate() {
        let attack = "WITH c AS (SELECT 1) /*!50000 INSERT INTO secrets VALUES (1) */";
        assert!(
            matches!(
                ensure_single_read_statement(attack),
                Err(SqlError::ExecutableComment)
            ),
            "dolt expands `/*!50000 ... */` and runs the insert, so the read \
             gate refuses executable comments outright (review round 2: no \
             lexical scan of the body is sound)"
        );
    }

    /// A quote inside a backtick identifier desynchronises literal blanking
    /// (#469, part 2).
    ///
    /// [`blank_string_literals`] tracks `'` and `"` but not backticks, so in
    /// `` WITH `'` AS ... `` the quote inside the identifier opens a "string"
    /// that swallows the INSERT keyword — the mutation scan sees a clean read.
    /// Dolt parses the backtick-quoted identifier and runs the insert.
    ///
    /// Verified against a real database (dolt 2.3.3, disposable fixture):
    ///
    /// ```text
    /// dolt sql -q "WITH `'` AS (SELECT 1) INSERT INTO notes VALUES ('bt')"
    /// -> row count 1 -> 2; the insert ran
    /// ```
    #[test]
    fn a_quote_inside_a_backtick_identifier_cannot_hide_a_cte_write() {
        let attack = "WITH `'` AS (SELECT 1) INSERT INTO secrets VALUES (1)";
        assert!(
            matches!(
                ensure_single_read_statement(attack),
                Err(SqlError::NotAReadStatement { .. })
            ),
            "the quote is part of an identifier, so dolt sees the INSERT and \
             runs it; the read gate must refuse this input"
        );
    }

    /// `DELETE FROM a , b USING a , b` writes to every named target (#469,
    /// part 3).
    ///
    /// The multi-target delete check reads only `tokens[2]`, so a comma
    /// surrounded by whitespace splits into its own token and the statement
    /// is authorised by its FIRST target alone.
    ///
    /// Verified against a real database (dolt 2.3.3, disposable fixture):
    /// with only `notes` allowlisted, the statement emptied BOTH tables
    /// (row counts 1 -> 0 in each).
    #[test]
    fn a_spaced_comma_delete_using_cannot_write_an_unlisted_table() {
        let allowed = vec!["notes".to_string()];
        let attack = "DELETE FROM notes , secrets USING notes , secrets";
        assert_eq!(
            table_written_by(attack, &allowed),
            Err(SqlError::MultiTableWrite),
            "the statement deletes from `secrets` too, so it must be refused \
             as a multi-table write"
        );
    }
}

#[cfg(test)]
mod review_round_2_tests {
    use super::*;

    /// Review round 2, finding 1a: a quote character inside an executable
    /// comment body must not desynchronize the gate's quote tracking into
    /// reading a real `;` as string data. Dolt's lexer ignores quotes inside
    /// `/*! ... */` while scanning for `*/` (live-verified: the second
    /// statement executes).
    #[test]
    fn a_quote_inside_an_executable_comment_cannot_hide_a_second_statement() {
        let sql = "SELECT 1 /*! -- '\n */; INSERT INTO secrets VALUES (9)";
        assert!(ensure_single_read_statement(sql).is_err());
    }

    /// Review round 2, finding 1a (backtick flavor) and 1b: the same
    /// poisoning applied to the write gate's mutating-keyword scan.
    #[test]
    fn a_backtick_inside_an_executable_comment_cannot_hide_a_second_statement() {
        let sql = "INSERT INTO notes VALUES ('a') /*! -- `\n */ ; delete from secrets";
        assert!(table_written_by(sql, &["notes".to_string()]).is_err());
    }

    /// Review round 2, finding 2: a comma glued into one token with a
    /// qualified name (`secrets,repo.notes`) is a multi-target DELETE; the
    /// final-segment rule must not launder it down to `notes`.
    #[test]
    fn a_glued_comma_qualified_delete_cannot_write_an_unlisted_table() {
        let sql = "DELETE FROM secrets,repo.notes USING secrets,repo.notes";
        assert!(table_written_by(sql, &["notes".to_string()]).is_err());
    }

    /// Review round 2, finding 3: a version prefix glued to the verb
    /// (`/*!50000INSERT`) hides the mutating keyword from the tokenizer when
    /// the body is scanned as live text.
    #[test]
    fn a_glued_version_prefix_cannot_hide_the_verb() {
        let sql = "WITH c AS (SELECT 1) /*!50000INSERT INTO secrets VALUES (7) */";
        assert!(ensure_single_read_statement(sql).is_err());
    }

    /// Benign companions (deny-side regressions to not reintroduce):
    /// an aliased single-table DELETE was admitted before this branch and
    /// must stay admitted — a space-separated trailing token is an alias,
    /// not a second table.
    #[test]
    fn an_aliased_single_table_delete_stays_admitted() {
        let sql = "DELETE FROM notes n WHERE n.id = 1";
        assert_eq!(
            table_written_by(sql, &["notes".to_string()]).unwrap(),
            "notes"
        );
        let sql_as = "DELETE FROM notes AS n WHERE n.id = 1";
        assert_eq!(
            table_written_by(sql_as, &["notes".to_string()]).unwrap(),
            "notes"
        );
    }

    /// A plain (non-executable) comment is still stripped as whitespace and
    /// must not be refused: `select 1 /* hint */ from notes` stays a read.
    #[test]
    fn a_plain_comment_stays_a_legal_read() {
        let sql = "SELECT 1 /* plain comment */ FROM notes";
        assert!(ensure_single_read_statement(sql).is_ok());
    }
}

/// Review round 3 (PR #473 coderabbit): a backtick-quoted table whose NAME
/// begins with a quote character is a DIFFERENT table. `` `'notes `` is the
/// table named `'notes`, not `notes`; blanket quote-trimming mis-classifies
/// it as the allowlisted name and admits the write.
#[test]
fn a_leading_quote_in_a_backticked_name_is_not_the_allowlisted_table() {
    let got = table_written_by("insert into `'notes` values (1)", &["notes".to_string()]);
    assert!(
        matches!(got, Err(SqlError::TableNotAllowed { .. })),
        "a distinct table whose name merely starts with a quote character \
         must not launder into the allowlisted name: {got:?}"
    );
}

#[test]
fn fully_quoted_tables_still_pass_the_allowlist() {
    // Sanity in both directions after the fix: proper quoting still trims.
    assert_eq!(
        table_written_by("insert into `notes` values (1)", &["notes".to_string()])
            .expect("quoted allowlisted table"),
        "notes"
    );
    assert_eq!(
        table_written_by("insert into notes values (1)", &["notes".to_string()])
            .expect("bare allowlisted table"),
        "notes"
    );
}

#[test]
fn qualified_quoted_names_trim_per_segment() {
    // `db`.`notes` style and db.`notes` style must both resolve to notes.
    assert_eq!(
        table_written_by("delete from `mydb`.`notes`", &["notes".to_string()])
            .expect("qualified quoted allowlisted table"),
        "notes"
    );
    assert_eq!(
        table_written_by("delete from mydb.`notes`", &["notes".to_string()])
            .expect("qualified quoted allowlisted table"),
        "notes"
    );
    // And the attack shape: db.`'notes` is the table 'notes in db mydb.
    let got = table_written_by("delete from mydb.`'notes`", &["notes".to_string()]);
    assert!(
        matches!(got, Err(SqlError::TableNotAllowed { .. })),
        "qualified quote-prefixed name must not launder: {got:?}"
    );
}

#[cfg(test)]
mod round5_tests {
    use super::*;
    // ---- review round 4 (independent re-review, gh#469): parse-boundary guards.
    // Each was RED on the lexical gate (round5-red.json) and must stay green here.

    /// Round-5 allowlist helper.
    fn allow() -> Vec<String> {
        vec!["notes".to_string()]
    }

    #[test]
    fn s1_quoted_name_containing_apostrophes_is_not_the_allowlisted_table() {
        // `'notes'` is a DISTINCT table whose own name contains apostrophes.
        // Live dolt wrote it after the lexical gate admitted `notes`
        // (blank_string_literals blanked the backticks; unquote() then stripped
        // the exposed apostrophe pair). The parser preserves the exact value.
        for sql in [
            "UPDATE `'notes'` SET id=9",
            "INSERT INTO `'notes'` VALUES (9)",
            "DELETE FROM `'notes'`",
        ] {
            let got = table_written_by(sql, &allow());
            assert!(
                matches!(got, Err(SqlError::TableNotAllowed { .. })),
                "quoted name containing apostrophes must not resolve to `notes` \
                 (`{sql}`): {got:?}"
            );
        }
    }

    #[test]
    fn s2_quoted_qualifier_insert_cannot_launder_the_target() {
        // `INSERT INTO `notes`.`secrets``: dolt writes `secrets` (database
        // `notes`); the lexical gate admitted target `notes` because only the
        // DELETE arm folded qualification. The parser resolves the final segment.
        let got = table_written_by("INSERT INTO `notes`.`secrets` VALUES (9)", &allow());
        assert!(
            matches!(got, Err(SqlError::TableNotAllowed { .. })),
            "quoted qualifier must resolve to its final segment `secrets`: {got:?}"
        );
    }

    #[test]
    fn s3_one_sided_comma_delete_is_multi_table() {
        // `DELETE FROM notes ,secrets USING notes,secrets` emptied BOTH tables
        // live: the comma-bearing token passed the lexical alias arm. The parser
        // sees two targets in the FROM list.
        let got = table_written_by("DELETE FROM notes ,secrets USING notes,secrets", &allow());
        assert!(
            matches!(got, Err(SqlError::MultiTableWrite)),
            "comma attached to either side is a target-list separator: {got:?}"
        );
        // Same for comma attached to the FIRST target.
        let got = table_written_by("DELETE FROM notes, secrets USING notes,secrets", &allow());
        assert!(matches!(got, Err(SqlError::MultiTableWrite)));
    }

    #[test]
    fn s4_hash_comment_carriage_return_cannot_smuggle_a_second_statement() {
        // dolt ends `#` comments at a carriage return; a gate stripping only to
        // newline sees one clean read while dolt runs the smuggled INSERT (live:
        // secrets 1->2 through the real read tool). Round 6 refuses `#` comments
        // outright (dolt's splitter also splits on `;` inside them in pure
        // ASCII), so this shape now trips HashComment before the CR is even
        // reached — the CR case alone is covered below.
        let sql = concat!(
            "SELECT 1# comment",
            "\u{000D}",
            "; INSERT INTO secrets VALUES (9)"
        );
        let got = ensure_single_read_statement(sql);
        assert!(
            matches!(got, Err(SqlError::HashComment)),
            "a `#` comment hides a real statement separator (with or without a CR): {got:?}"
        );
        // A bare carriage return outside any comment is still refused as a
        // control character (dolt ends `--` comments at CR too).
        let got = ensure_single_read_statement("select 1\r; delete from secrets");
        assert!(
            matches!(got, Err(SqlError::ControlCharacter)),
            "CR outside a comment is still a control-character refusal: {got:?}"
        );
    }

    #[test]
    fn l1_using_clause_alias_rebinding_is_multi_table() {
        // `DELETE FROM notes USING secrets AS notes` deletes every row of
        // `secrets` while the gate authorized `notes`. The parser names both
        // tables in the AST, so the write is refused as multi-table.
        let got = table_written_by("DELETE FROM notes USING secrets AS notes", &allow());
        assert!(
            matches!(got, Err(SqlError::MultiTableWrite)),
            "USING-clause alias can rebind the write target: {got:?}"
        );
        let got = table_written_by("DELETE FROM notes USING secrets", &allow());
        assert!(matches!(got, Err(SqlError::MultiTableWrite)));
    }

    #[test]
    fn l2_legitimate_qualified_writes_stay_admitted() {
        // Direction guard for the parse boundary: the legitimate qualified
        // single-table write that dolt accepts (and BASE admitted) must not be
        // over-refused by the rework.
        for sql in [
            "INSERT INTO `repo`.`notes` VALUES (9)",
            "UPDATE `repo`.`notes` SET id=9 WHERE 1=0",
        ] {
            let got = table_written_by(sql, &allow());
            assert_eq!(
                got.expect("legitimate qualified write must stay admitted"),
                "notes"
            );
        }
    }

    #[test]
    fn round5_read_path_contrasts() {
        // EXPLAIN-wrapped DML is not a read (L4): a version that executes
        // analyzed DML would bypass through the read path.
        assert!(matches!(
            ensure_single_read_statement("EXPLAIN ANALYZE INSERT INTO notes VALUES (9)"),
            Err(SqlError::NotAReadStatement { .. })
        ));
        // A WITH whose body mutates is a write regardless of the read-looking
        // preamble (CTE laundering through the parser's Query{body: Insert}).
        assert!(matches!(
            ensure_single_read_statement("WITH c AS (SELECT 1) INSERT INTO notes VALUES (9)"),
            Err(SqlError::NotAReadStatement { .. })
        ));
    }
}

#[cfg(test)]
mod round6_tests {
    use super::*;
    // ---- review round 5 (independent re-review, gh#469): guards for the
    // ---- findings that FAILED round 5. Each was RED on the round-5 head
    // ---- (round6-red.json) and must stay green here.

    fn allow() -> Vec<String> {
        vec!["notes".to_string()]
    }

    #[test]
    fn f1_ascii_hash_comment_cannot_smuggle_a_second_statement_read_path() {
        // dolt splits on `;` inside `#` comments; the parser sees one read.
        // Live on dolt 2.3.3: `select 1 # x ; delete from secrets` returned
        // the select's rows AND emptied secrets (review round 5).
        for sql in [
            "select 1 # x ; delete from secrets",
            "select 1#;delete from secrets",
            "show tables # x ; delete from secrets",
        ] {
            let got = ensure_single_read_statement(sql);
            assert!(
                matches!(got, Err(SqlError::HashComment)),
                "hash-comment `;` smuggling must be refused on the read path (`{sql}`): {got:?}"
            );
        }
    }

    #[test]
    fn f1_ascii_hash_comment_cannot_smuggle_a_second_statement_write_path() {
        // Live on dolt 2.3.3: this exact input was ADMITTED as a write to
        // `notes` and dolt executed the hidden DELETE (secrets 1->0).
        let got = table_written_by(
            "insert into notes values ('ok') # c ; delete from secrets",
            &allow(),
        );
        assert!(
            matches!(got, Err(SqlError::HashComment)),
            "hash-comment `;` smuggling must be refused on the write path: {got:?}"
        );
    }

    #[test]
    fn f1_unicode_comment_termination_cannot_smuggle_a_second_statement() {
        // dolt ends `#` comments at ANY non-ASCII codepoint (C1 controls,
        // U+00A0, U+00E9, U+2028/2029, CJK, emoji — review round 5
        // live-verified the class); the parser consumes them as comment
        // text. All non-ASCII input is refused.
        for sql in [
            concat!(
                "insert into notes values ('z') # c",
                "\u{2028}",
                "; delete from secrets"
            ),
            concat!("select 1 # c", "\u{00a0}", "; delete from secrets"),
            concat!("select 1 # c", "\u{00e9}", "; delete from secrets"),
        ] {
            // These shapes carry a `#`, so HashComment fires first — which is
            // exactly why the whole terminator family is closed. The remaining
            // exposure (non-ASCII outside any comment) is asserted below.
            let got = table_written_by(sql, &allow());
            assert!(
                got.is_err(),
                "non-ASCII comment terminator smuggling must be refused (`{sql:?}`): {got:?}"
            );
        }
        // Non-ASCII anywhere else is refused on its own ground: dolt ends `#`
        // comments at ANY non-ASCII codepoint, so the parser's view of where
        // a comment ends cannot be trusted in the presence of one at all.
        for sql in [
            concat!("select 1 -- c", "\u{2028}", "; delete from secrets"),
            "select '\u{00e9}' as x",
        ] {
            let got = ensure_single_read_statement(sql);
            assert!(
                matches!(got, Err(SqlError::NonAsciiCharacter)),
                "non-ASCII input must be refused wherever it appears (`{sql:?}`): {got:?}"
            );
        }
    }

    #[test]
    fn f2_cte_body_dml_cannot_smuggle_a_write_through_the_read_path() {
        // The round-5 head never inspected WITH CTE bodies. dolt 2.3.3
        // cannot parse DML CTEs today ("syntax error near 'INSERT'"), so
        // this is defense-in-depth for engines that accept them.
        for sql in [
            "WITH x AS (INSERT INTO secrets VALUES (1)) SELECT * FROM x",
            "WITH x AS (UPDATE notes SET id = 1) SELECT * FROM x",
        ] {
            let got = ensure_single_read_statement(sql);
            assert!(
                matches!(got, Err(SqlError::NotAReadStatement { .. })),
                "a CTE whose body mutates must not be admitted as a read (`{sql}`): {got:?}"
            );
        }
    }

    #[test]
    fn f3_select_into_table_is_not_a_read() {
        // `SELECT * INTO newtbl FROM notes` parses as Select.into = Some(..).
        // dolt 2.3.3 treats it as SELECT INTO @var and errors ("Undeclared
        // variable"), so no mutation today — but INTO names a table
        // destination and is refused on the read path.
        let got = ensure_single_read_statement("select * into newtbl from notes");
        assert!(
            matches!(got, Err(SqlError::NotAReadStatement { .. })),
            "SELECT ... INTO <table> names a write destination and must not be a read: {got:?}"
        );
    }

    #[test]
    fn over_refusals_documented_as_reads_are_re_admitted() {
        // Round 5 found `describe notes` (ExplainTable) and `show databases`
        // (ShowDatabases) refused, contradicting the module doc. Both are
        // read-only in dolt 2.3.3 (live rc=0, no row changes).
        for sql in ["describe notes", "show databases", "show schemas"] {
            let got = ensure_single_read_statement(sql);
            assert!(
                got.is_ok(),
                "`{sql}` is documented as a read and must be admitted: {got:?}"
            );
        }
    }

    #[test]
    fn hash_char_in_string_data_is_also_refused_by_design() {
        // The preflight is deliberately quote-unaware: a `#` anywhere — even
        // in a string literal — is refused, because a quote tracker is the
        // mechanism rounds 2–4 used to smuggle. Over-refusal in the safe
        // direction; pinned so a future edit cannot silently relax it to a
        // quote-aware scan without this test noticing the intent changed.
        let got = ensure_single_read_statement("select 'a # b' as x");
        assert!(
            matches!(got, Err(SqlError::HashComment)),
            "the `#` refusal is quote-unaware by design: {got:?}"
        );
    }

    #[test]
    fn non_ascii_in_string_data_is_also_refused_by_design() {
        let got = ensure_single_read_statement("select 'caf\u{00e9}' as x");
        assert!(
            matches!(got, Err(SqlError::NonAsciiCharacter)),
            "the non-ASCII refusal is quote-unaware by design: {got:?}"
        );
    }

    #[test]
    fn benign_reads_with_comments_and_ascii_stay_admitted() {
        // Direction guards: the two comment forms dolt and the parser AGREE
        // on (`--` to end of line, `/* */` block) stay admitted.
        for sql in [
            "select 1 -- ; delete from t",
            "select 1 /* ; */ from notes",
            "select 'a -- b' as x",
        ] {
            let got = ensure_single_read_statement(sql);
            assert!(
                got.is_ok(),
                "`{sql}` uses a comment form both lexers agree on and must stay admitted: {got:?}"
            );
        }
    }
}

#[cfg(test)]
mod round7_tests {
    use super::*;

    // ---- review round 6, check 2: nested WITH positions ----
    // dolt 2.3.3 rejects DML CTEs everywhere (rc=1, zero row changes,
    // live-verified in review round 6), so these are defense-in-depth for
    // engines that accept data-modifying CTEs. The round-6 head admitted all
    // five nested shapes because is_pure_read only inspected the TOP-LEVEL
    // query's `with`; a parenthesized query expression, a set-operation
    // operand, a derived table, a scalar subquery, and an INSERT ... SELECT
    // source each carry their own `Query` node whose `with` was never looked
    // at. The fix walks EVERY Query node in the parsed statement.

    const ALLOW: [&str; 1] = ["notes"];

    #[test]
    fn r6_nested_with_positions_are_refused_on_the_read_path() {
        for sql in [
            "(with x as (insert into secrets values (1)) select * from x)",
            "select 1 union all (with x as (insert into secrets values (1)) select * from x)",
            "select * from (with x as (insert into secrets values (1)) select 1) t",
            "select (with x as (insert into secrets values (1)) select 1)",
        ] {
            let got = ensure_single_read_statement(sql);
            assert!(
                got.is_err(),
                "a nested WITH carrying DML must not be admitted as a read (`{sql}`): {got:?}"
            );
        }
    }

    #[test]
    fn r6_insert_source_with_dml_cte_is_refused() {
        // Write path: the source query's `with` was equally uninspected.
        let got = table_written_by(
            "insert into notes with x as (insert into secrets values (1)) select 1",
            &ALLOW.map(String::from),
        );
        assert!(
            got.is_err(),
            "an INSERT whose source carries a DML CTE must be refused: {got:?}"
        );
    }

    #[test]
    fn r6_nested_pure_reads_stay_admitted() {
        for sql in [
            "(with x as (select 1 as one) select * from x)",
            "select 1 union all (with x as (select 2) select * from x)",
            "select * from (with x as (select 1) select * from x) t",
            "select (with x as (select 1) select 1)",
            "with x as (select 1) select * from x union all select 2",
        ] {
            let got = ensure_single_read_statement(sql);
            assert!(
                got.is_ok(),
                "a nested pure read must stay admitted (`{sql}`): {got:?}"
            );
        }
    }

    // ---- review round 6, docs: ControlCharacter message precision ----
    // dolt 2.3.3 ends `#` comments at a carriage return; `--` comments end
    // only at `\n`. The old message claimed CR ended both. Refusal of CR
    // itself is unchanged (still justified by `#` alone); only the stated
    // reason is corrected here — the test pins the corrected wording so the
    // message cannot silently regress to the disproven claim.

    #[test]
    fn r6_control_character_message_names_only_hash_comments() {
        let got = ensure_single_read_statement("select 1 # c\r; delete from secrets");
        match got {
            Err(SqlError::HashComment) => {} // # precedes the CR: fires first
            other => panic!("hash-comment-with-CR shape must refuse as HashComment: {other:?}"),
        }
        let bare_cr = ensure_single_read_statement("select 1\r; delete from secrets");
        assert!(
            matches!(bare_cr, Err(SqlError::ControlCharacter)),
            "bare CR shape must refuse as ControlCharacter: {bare_cr:?}"
        );
        let msg = SqlError::ControlCharacter.to_string();
        assert!(
            !msg.contains("`#`/`--`"),
            "the ControlCharacter message must not claim CR ends both comment kinds: {msg}"
        );
        assert!(
            msg.contains("`--` comments end only at a newline"),
            "the ControlCharacter message must state that `--` comments end only at a newline: {msg}"
        );
    }
}

#[cfg(test)]
mod round8_tests {
    use super::*;

    // ---- review round 7, F7-1: SHOW ... WHERE filters held uninspected
    // subqueries ----
    // The eight SHOW arms of is_pure_read were admitted with `=> true` and
    // never walked. dolt 2.3.3 DOES evaluate subqueries in SHOW WHERE (pure
    // probes rc=0, live-verified in review round 7) but parse-rejects every
    // mutation payload (rc=1, zero row changes) — defense-in-depth, same
    // class as round 6. The fix walks the whole STATEMENT before the shape
    // match, so SHOW filters are gated like every other position.

    const SHOW_DML_CTE_SHAPES: [&str; 8] = [
        "show tables where name in (with x as (insert into secrets values (1)) select 1)",
        "show columns from notes where field in (with x as (insert into secrets values (1)) select 1)",
        "show variables where variable_name in (with x as (insert into secrets values (1)) select 1)",
        "show status where variable_name in (with x as (insert into secrets values (1)) select 1)",
        "show functions where name in (with x as (insert into secrets values (1)) select 1)",
        "show databases where `database` in (with x as (insert into secrets values (1)) select 1)",
        "show schemas where `schema` in (with x as (insert into secrets values (1)) select 1)",
        "show collation where collation in (with x as (insert into secrets values (1)) select 1)",
    ];

    #[test]
    fn r7_show_where_dml_cte_is_refused_on_the_read_path() {
        for sql in SHOW_DML_CTE_SHAPES {
            let got = ensure_single_read_statement(sql);
            assert!(
                got.is_err(),
                "a SHOW whose WHERE filter carries a DML CTE must be refused (`{sql}`): {got:?}"
            );
        }
    }

    #[test]
    fn r7_show_where_body_insert_and_into_payloads_are_refused() {
        for sql in [
            "show tables where (with x as (select 1) insert into notes values (1))",
            "show tables where (select 1 into evil)",
            "show databases where (select 1 into evil)",
        ] {
            let got = ensure_single_read_statement(sql);
            assert!(
                got.is_err(),
                "a SHOW filter carrying a body-INSERT or SELECT-INTO payload must be refused (`{sql}`): {got:?}"
            );
        }
    }

    #[test]
    fn r7_pure_show_filters_stay_admitted() {
        // dolt 2.3.3 evaluates pure subqueries in SHOW WHERE (rc=0,
        // live-verified review round 7); the gate must not over-refuse them.
        for sql in [
            "show tables where name in (with x as (select 1) select 1)",
            "show tables where name in (select 'notes')",
            "show variables where variable_name = 'x'",
            "show databases",
            "describe notes",
            "explain select 1",
        ] {
            let got = ensure_single_read_statement(sql);
            assert!(
                got.is_ok(),
                "a pure SHOW/DESCRIBE/EXPLAIN must stay admitted (`{sql}`): {got:?}"
            );
        }
    }

    #[test]
    fn r7_write_path_nested_mutation_error_is_named() {
        let got = table_written_by(
            "insert into notes with x as (insert into secrets values (1)) select 1",
            &["notes".to_string()],
        );
        assert!(
            matches!(got, Err(SqlError::NestedMutation)),
            "the write-path nested-mutation refusal must be the named variant, not the read-tool message: {got:?}"
        );
    }
}

#[cfg(test)]
mod round9_tests {
    use super::*;

    // ---- review round 8, F8-1 (CRITICAL, live): sqlparser's SHOW fallback
    // (Statement::ShowVariable) runs parse_identifiers(), which swallows `;`
    // and everything after it as bare identifiers. `show engines; delete
    // from secrets` parsed as ONE admitted read while dolt executed BOTH
    // statements — secrets went 1->0 through the read tool's argv path
    // (live-verified twice on fresh fixtures). Base's lexical splitter
    // refused this family as MultipleStatements; the parse boundary
    // admitted it — a regression. Two independent guards close the class:
    // (a) ShowVariable (the unmodeled-variant fallback) is not a read;
    // (b) a single parsed statement whose TOKEN stream contains a
    //     non-trailing `;` is refused — the parse swallowed a statement
    //     boundary the engine will honor.

    #[test]
    fn r8_show_variable_fallback_is_not_a_read() {
        for sql in [
            "show engines",
            "show grants",
            "show character set",
            "show processlist",
            "show privileges",
            "show table status",
            "show procedure status",
            "show triggers",
            "show events",
            "show foo",
        ] {
            let got = ensure_single_read_statement(sql);
            assert!(
                matches!(got, Err(SqlError::NotAReadStatement { .. })),
                "the unmodeled SHOW fallback must not be admitted as a read (`{sql}`): {got:?}"
            );
        }
    }

    #[test]
    fn r8_swallowed_statement_boundaries_are_refused() {
        // F8-1 live table: every carrier with a stacked tail.
        for sql in [
            "show engines; delete from secrets",
            "show engines; insert into secrets values (2)",
            "show engines; replace into secrets values (5)",
            "show engines; truncate table secrets",
            "show engines; drop table secrets",
            "show engines; create table evil (id int)",
            "show grants; delete from secrets",
            "show character set; delete from secrets",
            "show processlist; delete from secrets",
            "show privileges; delete from secrets",
            "show table status; delete from secrets",
            "show procedure status where 1 in (select 1); delete from secrets",
            "show engines;delete from secrets",
        ] {
            let got = ensure_single_read_statement(sql);
            assert!(
                got.is_err(),
                "a `;` the parse swallowed must be refused (`{sql}`): {got:?}"
            );
        }
    }

    #[test]
    fn r8_trailing_semicolon_and_literal_semicolons_stay_admitted() {
        for sql in ["select 1;", "select 'a;b'"] {
            let read = ensure_single_read_statement(sql);
            assert!(
                read.is_ok(),
                "trailing `;` / literal `;` must stay admitted (`{sql}`): {read:?}"
            );
        }
        let write = table_written_by("insert into notes values ('a;b')", &["notes".to_string()]);
        assert!(
            write.is_ok(),
            "write with literal `;` must stay admitted: {write:?}"
        );
    }

    #[test]
    fn r8_flattened_show_where_payloads_are_refused() {
        // F8-2: unmodeled SHOW variants flatten WHERE subqueries into
        // identifier lists the walk cannot see. dolt 2.3.3 parse-rejects
        // these (rc=1, no-op) today — refusing the fallback arm (F8-1 fix)
        // closes this defense-in-depth gap too.
        for sql in [
            "show procedure status where 1 in (with x as (insert into secrets values (1)) select 1)",
            "show triggers where 1 in (with x as (insert into secrets values (1)) select 1)",
            "show events where 1 in (select 1 into evil)",
            "show table status where 1 in (with x as (insert into secrets values (1)) select 1)",
            "show (with x as (insert into secrets values (1)) select 1)",
        ] {
            let got = ensure_single_read_statement(sql);
            assert!(
                got.is_err(),
                "a flattened unmodeled-SHOW payload must be refused (`{sql}`): {got:?}"
            );
        }
    }
}

#[cfg(test)]
mod round10_tests {
    use super::*;

    // ---- post-round-9 review findings ----
    // REPLACE [INTO] parses as Statement::Insert with replace_into = true.
    // The write path's shape-only match admitted it even though the gate
    // documents INSERT/UPDATE/DELETE only, and REPLACE deletes conflicting
    // rows before inserting — a delete the allowlist never consented to.

    #[test]
    fn replace_statements_are_not_admitted_writes() {
        for sql in [
            "replace into notes values ('x')",
            "REPLACE INTO notes VALUES ('x')",
            "replace notes values ('x')",
            "replace into mydb.notes values ('x')",
        ] {
            let got = table_written_by(sql, &["notes".to_string()]);
            assert!(
                got.is_err(),
                "REPLACE is delete-then-insert and must not be an admitted write (`{sql}`): {got:?}"
            );
        }
    }

    #[test]
    fn plain_inserts_remain_admitted() {
        for sql in [
            "insert into notes values ('x')",
            "INSERT INTO notes VALUES ('x')",
            "insert into mydb.notes values ('x')",
            "insert into notes select * from notes",
        ] {
            let got = table_written_by(sql, &["notes".to_string()]);
            assert!(
                got.is_ok(),
                "plain INSERT must stay admitted (`{sql}`): {got:?}"
            );
        }
    }
}
