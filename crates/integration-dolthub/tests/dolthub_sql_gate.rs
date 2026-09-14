//! End-to-end: the dolthub adapter against a real Dolt database.
//!
//! The unit tests assert that the admission gate refuses stacked statements.
//! They cannot show that the refusal *matters* — that is a claim about what
//! `dolt sql -q` does with input the gate lets through. These tests create a
//! real repository, confirm experimentally that Dolt executes every statement
//! in a `-q` argument, and then confirm the tool refuses exactly that input.
//!
//! Every test here is `#[ignore]`d, following the same convention as the
//! Qdrant integration suite (#358): an early `return` on a missing binary is
//! reported by the harness as `ok`, and the `eprintln!` explaining why is
//! hidden for passing tests — so the whole file can silently never run while
//! the summary reads green. `#[ignore]` makes non-execution visible in the
//! count, and CI runs them with `--ignored` where `dolt` is installed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use ardur_integration_dolthub::DolthubTool;
use ardur_tool_registry::{CapTokenRef, InvocationId, SessionId, Tool, ToolContext};
use serde_json::json;

/// The `dolt` binary, or `None` when it is not installed.
fn dolt_binary() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("dolt"))
        .find(|c| c.is_file())
}

/// A real Dolt repository with one table and one row.
///
/// The repository and Dolt's own config root are **separate directories**.
/// Pointing `DOLT_ROOT_PATH` at the repository makes Dolt create its config
/// `.dolt` directory there, after which `dolt init` refuses with ".dolt
/// directory already exists" — which is how the first version of this fixture
/// silently skipped every test in the file.
fn dolt_repo(dolt: &Path) -> Option<(tempfile::TempDir, PathBuf, PathBuf)> {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&home).ok()?;
    std::fs::create_dir_all(&repo).ok()?;

    let run = |args: &[&str]| -> bool {
        Command::new(dolt)
            .args(args)
            .current_dir(&repo)
            // Keep a developer's real dolt identity and config out of the test.
            .env("DOLT_ROOT_PATH", &home)
            .env("HOME", &home)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };

    if !run(&["init", "--name", "Test", "--email", "test@example.invalid"]) {
        return None;
    }
    if !run(&[
        "sql",
        "-q",
        "create table notes (id varchar(8) primary key)",
    ]) {
        return None;
    }
    if !run(&["sql", "-q", "insert into notes values ('keep')"]) {
        return None;
    }
    Some((dir, repo, home))
}

/// Set up, or explain why the test cannot run.
macro_rules! dolt_fixture {
    () => {{
        // These tests are `#[ignore]`d, so reaching this point means someone
        // asked for them explicitly. A missing binary is then a real failure,
        // not something to skip past.
        let dolt = dolt_binary().expect(
            "`dolt` is not installed, but these tests were run with --ignored; \
             install dolt or drop the --ignored flag",
        );
        match dolt_repo(&dolt) {
            Some((guard, repo, home)) => (dolt, guard, repo, home),
            None => panic!(
                "`dolt` is installed but the fixture could not be initialised — \
                 this is a broken test, not a missing toolchain"
            ),
        }
    }};
}

fn ctx(repo: &Path, home: &Path) -> ToolContext {
    let mut env = HashMap::new();
    env.insert("HOME".to_string(), home.display().to_string());
    env.insert("DOLT_ROOT_PATH".to_string(), home.display().to_string());
    ToolContext {
        cap_token: CapTokenRef(String::new()),
        session_id: SessionId::new(),
        invocation_id: InvocationId::new(),
        cwd: repo.to_path_buf(),
        env,
        cost_budget_cents: u32::MAX,
    }
}

fn row_count(dolt: &Path, repo: &Path, home: &Path) -> usize {
    let out = Command::new(dolt)
        .args(["sql", "-q", "select count(*) as n from notes", "-r", "json"])
        .current_dir(repo)
        .env("DOLT_ROOT_PATH", home)
        .env("HOME", home)
        .output()
        .expect("count runs");
    let text = String::from_utf8_lossy(&out.stdout);
    // `{"rows": [{"n":1}]}`
    text.split("\"n\":")
        .nth(1)
        .and_then(|t| t.trim_start().split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("could not read count from: {text}"))
}

#[tokio::test]
#[ignore = "needs the `dolt` binary; run with --ignored"]
async fn dolt_really_executes_stacked_statements() {
    // The premise of the whole admission gate, established rather than assumed.
    // If this ever stops being true the gate is stricter than it needs to be —
    // but it will never be weaker than it needs to be, which is the direction
    // that matters.
    let (dolt, _guard, repo, home) = dolt_fixture!();
    assert_eq!(row_count(&dolt, &repo, &home), 1, "one row to begin with");

    let out = Command::new(&dolt)
        .args([
            "sql",
            "-q",
            "select 1; insert into notes values ('stacked')",
            "-r",
            "json",
        ])
        .current_dir(&repo)
        .env("DOLT_ROOT_PATH", &home)
        .env("HOME", &home)
        .output()
        .expect("dolt runs");
    assert!(out.status.success(), "dolt accepted the stacked input");

    assert_eq!(
        row_count(&dolt, &repo, &home),
        2,
        "dolt executes EVERY statement in a -q argument — this is why the read \
         tool must refuse multi-statement input rather than inspecting only the \
         leading verb"
    );
}

#[tokio::test]
#[ignore = "needs the `dolt` binary; run with --ignored"]
async fn the_read_tool_refuses_the_stacked_write_that_dolt_would_execute() {
    let (dolt, _guard, repo, home) = dolt_fixture!();
    let before = row_count(&dolt, &repo, &home);

    let tool = DolthubTool::read(dolt.to_string_lossy().to_string(), vec![]);
    let result = tool
        .invoke(
            &ctx(&repo, &home),
            json!({ "sql": "select 1; insert into notes values ('pwned')" }),
        )
        .await;

    assert!(result.is_err(), "the stacked write must be refused");
    assert_eq!(
        row_count(&dolt, &repo, &home),
        before,
        "and nothing may have been written"
    );
}

#[tokio::test]
#[ignore = "needs the `dolt` binary; run with --ignored"]
async fn a_plain_select_returns_rows() {
    let (dolt, _guard, repo, home) = dolt_fixture!();

    let tool = DolthubTool::read(dolt.to_string_lossy().to_string(), vec![]);
    let output = tool
        .invoke(&ctx(&repo, &home), json!({ "sql": "select id from notes" }))
        .await
        .expect("a plain select succeeds");

    let stdout = output.content["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("keep"),
        "the query must return the row: {stdout}"
    );
}

#[tokio::test]
#[ignore = "needs the `dolt` binary; run with --ignored"]
async fn a_write_to_an_unlisted_table_never_reaches_dolt() {
    let (dolt, _guard, repo, home) = dolt_fixture!();
    let before = row_count(&dolt, &repo, &home);

    // `notes` exists and the statement is valid, so only the allowlist stands
    // between it and a successful write.
    let tool = DolthubTool::write(
        dolt.to_string_lossy().to_string(),
        vec!["something_else".to_string()],
        vec![],
    );
    let result = tool
        .invoke(
            &ctx(&repo, &home),
            json!({ "sql": "insert into notes values ('sneaky')" }),
        )
        .await;

    assert!(result.is_err(), "an unlisted table must be refused");
    assert_eq!(
        row_count(&dolt, &repo, &home),
        before,
        "the refusal must happen before dolt runs, so no row appears"
    );
}

#[tokio::test]
#[ignore = "needs the `dolt` binary; run with --ignored"]
async fn a_write_to_an_allowlisted_table_lands() {
    let (dolt, _guard, repo, home) = dolt_fixture!();
    let before = row_count(&dolt, &repo, &home);

    let tool = DolthubTool::write(
        dolt.to_string_lossy().to_string(),
        vec!["notes".to_string()],
        vec![],
    );
    let output = tool
        .invoke(
            &ctx(&repo, &home),
            json!({ "sql": "insert into notes values ('added')" }),
        )
        .await
        .expect("an allowlisted write succeeds");

    assert_eq!(
        row_count(&dolt, &repo, &home),
        before + 1,
        "the row must actually land, or the allowlist is merely decorative"
    );
    assert_eq!(output.receipt_data["integration"], "dolthub");
    assert_eq!(output.receipt_data["verb"], "execute");
}

/// The backslash-escape bypass, end to end against a real database.
///
/// Dolt treats `\'` as a literal quote, so in `select 'a\'' ; insert ...` the
/// string closes at the third quote and the `;` is a real separator. A checker
/// that toggles quote state on every `'` concludes the opposite and reads the
/// separator as string data — admitting an input whose insert dolt then runs.
///
/// The first version of this gate had exactly that bug. This test would have
/// caught it: it asserts both that dolt performs the write and that the tool
/// refuses the input.
#[tokio::test]
#[ignore = "needs the `dolt` binary; run with --ignored"]
async fn a_backslash_escaped_quote_cannot_smuggle_a_write_past_the_read_tool() {
    let (dolt, _guard, repo, home) = dolt_fixture!();

    // First: establish that dolt really does execute the smuggled insert, so
    // the refusal below is known to matter.
    let before = row_count(&dolt, &repo, &home);
    let out = Command::new(&dolt)
        .args([
            "sql",
            "-q",
            r"select 'a\'' ; insert into notes values ('smuggled')",
            "-r",
            "json",
        ])
        .current_dir(&repo)
        .env("DOLT_ROOT_PATH", &home)
        .env("HOME", &home)
        .output()
        .expect("dolt runs");
    assert!(out.status.success(), "dolt accepted the escaped input");
    assert_eq!(
        row_count(&dolt, &repo, &home),
        before + 1,
        "dolt closes the string at the escaped quote and runs the insert"
    );

    // Now the tool must refuse that same input.
    let after_probe = row_count(&dolt, &repo, &home);
    let tool = DolthubTool::read(dolt.to_string_lossy().to_string(), vec![]);
    let result = tool
        .invoke(
            &ctx(&repo, &home),
            json!({ "sql": r"select 'a\'' ; insert into notes values ('pwned')" }),
        )
        .await;

    assert!(
        result.is_err(),
        "the read tool must refuse an input dolt would use to write"
    );
    assert_eq!(
        row_count(&dolt, &repo, &home),
        after_probe,
        "and no row may have been added"
    );
}

/// Row count of an arbitrary table, for the #469 tests that protect a second
/// table the shared fixture does not create.
fn table_count(dolt: &Path, repo: &Path, home: &Path, table: &str) -> usize {
    let out = Command::new(dolt)
        .args([
            "sql",
            "-q",
            &format!("select count(*) as n from {table}"),
            "-r",
            "json",
        ])
        .current_dir(repo)
        .env("DOLT_ROOT_PATH", home)
        .env("HOME", home)
        .output()
        .expect("count runs");
    let text = String::from_utf8_lossy(&out.stdout);
    text.split("\"n\":")
        .nth(1)
        .and_then(|t| t.trim_start().split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("could not read count from: {text}"))
}

/// Create and seed the `secrets` table the #469 tests protect.
///
/// Only ever called inside a `dolt_fixture!()` tempdir.
fn seed_secrets(dolt: &Path, repo: &Path, home: &Path) {
    let out = Command::new(dolt)
        .args([
            "sql",
            "-q",
            "create table secrets (id varchar(16) primary key)",
        ])
        .current_dir(repo)
        .env("DOLT_ROOT_PATH", home)
        .env("HOME", home)
        .output()
        .expect("create secrets runs");
    assert!(out.status.success(), "fixture: create secrets failed");
    let out = Command::new(dolt)
        .args(["sql", "-q", "insert into secrets values ('s')"])
        .current_dir(repo)
        .env("DOLT_ROOT_PATH", home)
        .env("HOME", home)
        .output()
        .expect("seed secrets runs");
    assert!(out.status.success(), "fixture: seed secrets failed");
}

/// The executable-comment bypass (#469 part 1), end to end.
///
/// `/*!50000 ... */` is MySQL conditional-execution syntax: dolt expands it
/// and runs its body. `strip_comments` treats it as an inert comment, so the
/// read gate admits the input — and dolt executes the INSERT it hides.
#[tokio::test]
#[ignore = "needs the `dolt` binary; run with --ignored"]
async fn an_executable_comment_cannot_smuggle_a_write_past_the_read_tool() {
    let (dolt, _guard, repo, home) = dolt_fixture!();
    seed_secrets(&dolt, &repo, &home);

    // First: prove dolt executes the hidden insert, so the refusal below is
    // known to matter.
    let before = table_count(&dolt, &repo, &home, "secrets");
    let out = Command::new(&dolt)
        .args([
            "sql",
            "-q",
            "WITH c AS (SELECT 1) /*!50000 INSERT INTO secrets VALUES ('smuggled') */",
            "-r",
            "json",
        ])
        .current_dir(&repo)
        .env("DOLT_ROOT_PATH", &home)
        .env("HOME", &home)
        .output()
        .expect("dolt runs");
    assert!(out.status.success(), "dolt accepted the executable comment");
    assert_eq!(
        table_count(&dolt, &repo, &home, "secrets"),
        before + 1,
        "dolt expands `/*!50000 ... */` and runs the hidden insert"
    );

    // Now the read tool must refuse the same shape of input.
    let after_probe = table_count(&dolt, &repo, &home, "secrets");
    let tool = DolthubTool::read(dolt.to_string_lossy().to_string(), vec![]);
    let result = tool
        .invoke(
            &ctx(&repo, &home),
            json!({ "sql": "WITH c AS (SELECT 1) /*!50000 INSERT INTO secrets VALUES ('pwned') */" }),
        )
        .await;

    assert!(
        result.is_err(),
        "the read tool must refuse an input whose executable comment writes"
    );
    assert_eq!(
        table_count(&dolt, &repo, &home, "secrets"),
        after_probe,
        "and no row may have been added"
    );
}

/// The backtick-identifier bypass (#469 part 2), end to end.
///
/// In `` WITH `'` AS ... `` the quote is part of the identifier. Literal
/// blanking does not track backticks, so it treats everything after that
/// quote as string data, the mutation scan sees a clean read, and dolt runs
/// the INSERT.
#[tokio::test]
#[ignore = "needs the `dolt` binary; run with --ignored"]
async fn a_backtick_identifier_quote_cannot_smuggle_a_cte_write_past_the_read_tool() {
    let (dolt, _guard, repo, home) = dolt_fixture!();
    seed_secrets(&dolt, &repo, &home);

    // First: prove dolt executes the CTE insert behind the odd identifier.
    let before = table_count(&dolt, &repo, &home, "secrets");
    let out = Command::new(&dolt)
        .args([
            "sql",
            "-q",
            "WITH `'` AS (SELECT 1) INSERT INTO secrets VALUES ('bt')",
            "-r",
            "json",
        ])
        .current_dir(&repo)
        .env("DOLT_ROOT_PATH", &home)
        .env("HOME", &home)
        .output()
        .expect("dolt runs");
    assert!(out.status.success(), "dolt accepted the backtick input");
    assert_eq!(
        table_count(&dolt, &repo, &home, "secrets"),
        before + 1,
        "dolt parses the backtick identifier and runs the insert"
    );

    // Now the read tool must refuse the same shape of input.
    let after_probe = table_count(&dolt, &repo, &home, "secrets");
    let tool = DolthubTool::read(dolt.to_string_lossy().to_string(), vec![]);
    let result = tool
        .invoke(
            &ctx(&repo, &home),
            json!({ "sql": "WITH `'` AS (SELECT 1) INSERT INTO secrets VALUES ('pwned')" }),
        )
        .await;

    assert!(
        result.is_err(),
        "the read tool must refuse an input whose identifier-embedded quote \
         hides a write"
    );
    assert_eq!(
        table_count(&dolt, &repo, &home, "secrets"),
        after_probe,
        "and no row may have been added"
    );
}

/// The spaced-comma multi-target DELETE bypass (#469 part 3), end to end.
///
/// `DELETE FROM notes , secrets USING notes , secrets` deletes from BOTH
/// tables. The write gate's comma check looks at a single token, so the
/// whitespace around the comma defeats it and the statement is authorised by
/// `notes` alone — with `secrets` never checked against the allowlist.
#[tokio::test]
#[ignore = "needs the `dolt` binary; run with --ignored"]
async fn a_spaced_comma_delete_using_cannot_reach_an_unlisted_table_through_the_write_tool() {
    let (dolt, _guard, repo, home) = dolt_fixture!();
    seed_secrets(&dolt, &repo, &home);

    // First: prove dolt executes this as a multi-target delete.
    let notes_before = table_count(&dolt, &repo, &home, "notes");
    let out = Command::new(&dolt)
        .args([
            "sql",
            "-q",
            "DELETE FROM notes , secrets USING notes , secrets",
            "-r",
            "json",
        ])
        .current_dir(&repo)
        .env("DOLT_ROOT_PATH", &home)
        .env("HOME", &home)
        .output()
        .expect("dolt runs");
    assert!(
        out.status.success(),
        "dolt accepted the multi-target delete"
    );
    assert_eq!(
        table_count(&dolt, &repo, &home, "notes"),
        0,
        "the probe emptied notes (had {notes_before})"
    );
    assert_eq!(
        table_count(&dolt, &repo, &home, "secrets"),
        0,
        "the probe emptied secrets too — the statement writes to a table \
         the FROM clause's first target hides"
    );

    // Re-seed so the refusal below has rows to protect, then require the
    // write tool (allowing only `notes`) to refuse the identical statement.
    for table in ["notes", "secrets"] {
        let out = Command::new(&dolt)
            .args([
                "sql",
                "-q",
                &format!("insert into {table} values ('again')"),
            ])
            .current_dir(&repo)
            .env("DOLT_ROOT_PATH", &home)
            .env("HOME", &home)
            .output()
            .expect("re-seed runs");
        assert!(out.status.success(), "fixture: re-seed {table} failed");
    }

    let tool = DolthubTool::write(
        dolt.to_string_lossy().to_string(),
        vec!["notes".to_string()],
        vec![],
    );
    let result = tool
        .invoke(
            &ctx(&repo, &home),
            json!({ "sql": "DELETE FROM notes , secrets USING notes , secrets" }),
        )
        .await;

    assert!(
        result.is_err(),
        "the write tool must refuse a delete that also targets an unlisted table"
    );
    assert_eq!(
        table_count(&dolt, &repo, &home, "secrets"),
        1,
        "the unlisted table must still hold its row"
    );
    assert_eq!(
        table_count(&dolt, &repo, &home, "notes"),
        1,
        "the refusal must happen before dolt runs, so notes is untouched too"
    );
}

// ---------------------------------------------------------------------------
// Review round 2: live-verified bypass shapes against real dolt.
// ---------------------------------------------------------------------------

/// A quote inside an executable comment poisons lexical quote tracking while
/// dolt's own lexer ignores it while scanning for `*/` — the smuggled second
/// statement runs (live-verified rc=0, row inserted). The read tool must
/// refuse the identical input.
#[tokio::test]
#[ignore = "needs the `dolt` binary; run with --ignored"]
async fn a_poisoned_executable_comment_cannot_smuggle_a_write_past_the_read_tool() {
    let (dolt, _guard, repo, home) = dolt_fixture!();
    seed_secrets(&dolt, &repo, &home);

    let smuggled = "SELECT 1 /*! -- '\n */; INSERT INTO secrets VALUES ('smuggled')";
    let out = Command::new(&dolt)
        .args(["sql", "-q", smuggled, "-r", "json"])
        .current_dir(&repo)
        .env("DOLT_ROOT_PATH", &home)
        .env("HOME", &home)
        .output()
        .expect("dolt runs");
    assert!(
        out.status.success(),
        "precondition: raw dolt executes the smuggled insert past the poisoned comment"
    );
    assert_eq!(
        table_count(&dolt, &repo, &home, "secrets"),
        2,
        "precondition: the row was actually inserted"
    );

    // The probe mutated the fixture; reset for the refusal half.
    Command::new(&dolt)
        .args(["sql", "-q", "delete from secrets"])
        .current_dir(&repo)
        .env("DOLT_ROOT_PATH", &home)
        .env("HOME", &home)
        .output()
        .expect("reset runs");
    Command::new(&dolt)
        .args(["sql", "-q", "insert into secrets values ('s')"])
        .current_dir(&repo)
        .env("DOLT_ROOT_PATH", &home)
        .env("HOME", &home)
        .output()
        .expect("reseed runs");

    let tool = DolthubTool::read(dolt.to_string_lossy().to_string(), vec![]);
    let result = tool
        .invoke(&ctx(&repo, &home), json!({ "sql": smuggled }))
        .await;
    assert!(
        result.is_err(),
        "the read tool must refuse input whose executable comment hides a second statement"
    );
    assert_eq!(
        table_count(&dolt, &repo, &home, "secrets"),
        1,
        "protected rows unchanged — the insert must NOT have run"
    );
}

/// Glued-comma multi-target DELETE (`secrets,db.notes` as ONE token):
/// live-verified to empty BOTH tables (rc=0). The write tool allowlisting
/// only `notes` must refuse it.
#[tokio::test]
#[ignore = "needs the `dolt` binary; run with --ignored"]
async fn a_glued_comma_delete_cannot_write_an_unlisted_table_for_real() {
    let (dolt, _guard, repo, home) = dolt_fixture!();
    seed_secrets(&dolt, &repo, &home);
    // The fixture's `notes` table already exists with one row.

    // The database name qualifies the second target so both tables resolve.
    let db_out = Command::new(&dolt)
        .args(["sql", "-q", "select database() as db", "-r", "json"])
        .current_dir(&repo)
        .env("DOLT_ROOT_PATH", &home)
        .env("HOME", &home)
        .output()
        .expect("db name runs");
    let db_text = String::from_utf8_lossy(&db_out.stdout);
    let db = db_text
        .split("\"db\":")
        .nth(1)
        .and_then(|t| t.trim_start().split('"').nth(1))
        .expect("database name readable")
        .to_string();

    let sql = format!("DELETE FROM secrets,{db}.notes USING secrets,{db}.notes");
    let out = Command::new(&dolt)
        .args(["sql", "-q", &sql, "-r", "json"])
        .current_dir(&repo)
        .env("DOLT_ROOT_PATH", &home)
        .env("HOME", &home)
        .output()
        .expect("dolt runs");
    assert!(
        out.status.success()
            && table_count(&dolt, &repo, &home, "secrets") == 0
            && table_count(&dolt, &repo, &home, "notes") == 0,
        "precondition: raw dolt empties BOTH tables"
    );

    // Reset both tables for the refusal half.
    for table in ["secrets", "notes"] {
        Command::new(&dolt)
            .args(["sql", "-q", &format!("insert into {table} values ('x')")])
            .current_dir(&repo)
            .env("DOLT_ROOT_PATH", &home)
            .env("HOME", &home)
            .output()
            .expect("reseed runs");
    }

    let tool = DolthubTool::write(
        dolt.to_string_lossy().to_string(),
        vec!["notes".to_string()],
        vec![],
    );
    let result = tool.invoke(&ctx(&repo, &home), json!({ "sql": sql })).await;
    assert!(
        result.is_err(),
        "the write tool must refuse a glued-comma multi-target delete"
    );
    assert_eq!(
        table_count(&dolt, &repo, &home, "secrets"),
        1,
        "unlisted secrets rows must be untouched"
    );
    assert_eq!(table_count(&dolt, &repo, &home, "notes"), 1);
}

// ---------------------------------------------------------------------------
// Review round 5 (gh#469): the `#`-comment statement-splitting bypass.
// ---------------------------------------------------------------------------

/// dolt's statement splitter splits on `;` inside `#` comments while a
/// standard parser treats `# ...` as a comment to end of line. Live on
/// dolt 2.3.3, this exact input inserted the row AND emptied `secrets`.
/// The read tool must refuse the identical input.
#[tokio::test]
#[ignore = "needs the `dolt` binary; run with --ignored"]
async fn a_hash_comment_cannot_smuggle_a_delete_past_the_read_tool() {
    let (dolt, _guard, repo, home) = dolt_fixture!();
    seed_secrets(&dolt, &repo, &home);

    // First: prove the engine really splits inside the `#` comment, so the
    // refusal below is known to matter.
    let before = table_count(&dolt, &repo, &home, "secrets");
    let out = Command::new(&dolt)
        .args([
            "sql",
            "-q",
            "select 1 # x ; delete from secrets",
            "-r",
            "json",
        ])
        .current_dir(&repo)
        .env("DOLT_ROOT_PATH", &home)
        .env("HOME", &home)
        .output()
        .expect("dolt runs");
    assert!(
        out.status.success(),
        "precondition: dolt accepts the hash-comment input"
    );
    assert_eq!(
        table_count(&dolt, &repo, &home, "secrets"),
        0,
        "precondition: dolt splits on `;` inside `#` comments and ran the hidden delete (had {before})"
    );

    // Reset for the refusal half.
    Command::new(&dolt)
        .args(["sql", "-q", "insert into secrets values ('s')"])
        .current_dir(&repo)
        .env("DOLT_ROOT_PATH", &home)
        .env("HOME", &home)
        .output()
        .expect("reseed runs");

    let tool = DolthubTool::read(dolt.to_string_lossy().to_string(), vec![]);
    let result = tool
        .invoke(
            &ctx(&repo, &home),
            json!({ "sql": "select 1 # x ; delete from secrets" }),
        )
        .await;
    assert!(
        result.is_err(),
        "the read tool must refuse input whose `#` comment hides a separator"
    );
    assert_eq!(
        table_count(&dolt, &repo, &home, "secrets"),
        1,
        "protected rows unchanged — the hidden delete must NOT have run"
    );
}

/// The write-path flavour: allowlisted INSERT, hidden DELETE. The write tool
/// must refuse it even though the visible target is allowlisted.
#[tokio::test]
#[ignore = "needs the `dolt` binary; run with --ignored"]
async fn a_hash_comment_cannot_smuggle_a_delete_past_the_write_tool() {
    let (dolt, _guard, repo, home) = dolt_fixture!();
    seed_secrets(&dolt, &repo, &home);

    let tool = DolthubTool::write(
        dolt.to_string_lossy().to_string(),
        vec!["notes".to_string()],
        vec![],
    );
    let result = tool
        .invoke(
            &ctx(&repo, &home),
            json!({ "sql": "insert into notes values ('ok') # c ; delete from secrets" }),
        )
        .await;
    assert!(
        result.is_err(),
        "the write tool must refuse a `#` comment hiding a second statement"
    );
    assert_eq!(
        table_count(&dolt, &repo, &home, "secrets"),
        1,
        "the hidden delete must not have run"
    );
    assert_eq!(
        table_count(&dolt, &repo, &home, "notes"),
        1,
        "the visible insert must not have run either — refusal precedes execution"
    );
}

/// The swallowed-separator bypass (review round 8, F8-1), end to end.
///
/// sqlparser's SHOW fallback (`Statement::ShowVariable`) flattens everything
/// after `show` into bare identifiers, swallowing the `;` — so
/// `show engines; delete from secrets` parsed as ONE admitted read while
/// dolt executed BOTH statements (live-verified: secrets 1->0, twice, on
/// fresh fixtures). The swallow guard in `parse_one` counts `;` tokens
/// (whitespace-trimmed) and refuses any non-trailing separator.
#[tokio::test]
#[ignore = "needs the `dolt` binary; run with --ignored"]
async fn the_show_variable_swallowed_separator_bypass_is_refused() {
    let (dolt, _guard, repo, home) = dolt_fixture!();
    seed_secrets(&dolt, &repo, &home);

    // Engine premise: dolt executes the stacked pair as two statements.
    let out = Command::new(&dolt)
        .args(["sql", "-q", "show engines; delete from secrets"])
        .current_dir(&repo)
        .env("DOLT_ROOT_PATH", &home)
        .env("HOME", &home)
        .output()
        .expect("dolt runs");
    assert!(
        out.status.success(),
        "engine premise broken: dolt must execute the stacked pair"
    );
    assert_eq!(
        table_count(&dolt, &repo, &home, "secrets"),
        0,
        "dolt ran the delete the parser swallowed"
    );

    // The fixture's secrets are now empty; re-seed and prove the gate
    // refuses the identical input and the table survives.
    let out = Command::new(&dolt)
        .args(["sql", "-q", "insert into secrets values ('s')"])
        .current_dir(&repo)
        .env("DOLT_ROOT_PATH", &home)
        .env("HOME", &home)
        .output()
        .expect("reseed runs");
    assert!(out.status.success(), "fixture: reseed failed");

    let tool = DolthubTool::read(dolt.to_string_lossy().to_string(), vec![]);
    let result = tool
        .invoke(
            &ctx(&repo, &home),
            json!({ "sql": "show engines; delete from secrets" }),
        )
        .await;
    assert!(
        result.is_err(),
        "the read tool must refuse a swallowed statement boundary"
    );
    assert_eq!(
        table_count(&dolt, &repo, &home, "secrets"),
        1,
        "the stacked delete must not have run through the tool"
    );
}
