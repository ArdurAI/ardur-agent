//! End-to-end: the dolthub adapter against a real Dolt database.
//!
//! The unit tests assert that the admission gate refuses stacked statements.
//! They cannot show that the refusal *matters* — that is a claim about what
//! `dolt sql -q` does with input the gate lets through. These tests create a
//! real repository, confirm experimentally that Dolt executes every statement
//! in a `-q` argument, and then confirm the tool refuses exactly that input.
//!
//! Skipped when `dolt` is absent, because a missing toolchain is not a test
//! failure — but a skip is loud and distinguishes "not installed" from "setup
//! broke", so a suite that silently never runs is visible rather than passing
//! vacuously.

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
        let Some(dolt) = dolt_binary() else {
            eprintln!("SKIPPED: `dolt` is not installed");
            return;
        };
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
