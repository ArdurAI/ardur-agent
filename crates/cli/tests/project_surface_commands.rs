//! Integration tests for `ardur project` Kanban + run ledger commands.

use assert_cmd::Command;
use serde_json::Value;

#[test]
fn project_surface_card_and_run_lifecycle() {
    let dir = tempfile::tempdir().expect("tempdir");

    let add = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args([
            "project",
            "add-card",
            "Wire signed marketplace",
            "--status",
            "ready",
            "--owner",
            "agent-a",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let add_stdout = String::from_utf8(add).expect("stdout utf8");
    let card_id = add_stdout
        .split_whitespace()
        .last()
        .expect("card id")
        .trim()
        .to_string();

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["project", "move", &card_id, "in-review"])
        .assert()
        .success();

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args([
            "project",
            "record-run",
            "--agent",
            "codex-worker",
            "--summary",
            "implemented and tested",
            "--receipt",
            "https://github.com/ArdurAI/ardur-agent/pull/232",
            "--card",
            &card_id,
            "--status",
            "completed",
        ])
        .assert()
        .success();

    let board = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["project", "board"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let board: Value = serde_json::from_slice(&board).expect("board json");
    assert_eq!(board["cards"][0]["id"], card_id);
    assert_eq!(board["cards"][0]["status"], "in-review");
    assert_eq!(board["cards"][0]["owner"], "agent-a");
    assert_eq!(board["runs"][0]["agent"], "codex-worker");
    assert_eq!(board["runs"][0]["card"], card_id);

    let path = dir.path().join(".ardur").join("project-surface.json");
    assert!(path.is_file(), "project surface should be persisted");
}

#[test]
fn project_surface_rejects_run_for_unknown_card() {
    let dir = tempfile::tempdir().expect("tempdir");
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args([
            "project",
            "record-run",
            "--agent",
            "worker",
            "--summary",
            "no card",
            "--receipt",
            "receipt://1",
            "--card",
            "missing-card",
        ])
        .assert()
        .failure();
}
