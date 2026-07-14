//! Integration tests for `ardur schedule` (§9.4 cron management surface).

use assert_cmd::Command;

/// A fresh temp `HOME`, canonicalized: the CLI's `secure_io` module resolves
/// every path component descriptor-relatively with `O_NOFOLLOW`, so a macOS
/// `$TMPDIR` path through the `/var` -> `/private/var` symlink must be
/// canonicalized first (a real `~/.ardur` root is already canonical).
fn canonical_tempdir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().canonicalize().expect("canonicalize tempdir");
    (dir, path)
}

fn create_write_token(home: &std::path::Path) -> String {
    let output = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", home)
        .args(["token", "create", "operator", "--scope", "write"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout utf8");
    stdout
        .lines()
        .find(|l| l.starts_with("value:"))
        .and_then(|l| l.strip_prefix("value: "))
        .expect("token value in stdout")
        .to_string()
}

#[test]
fn schedule_create_list_next_delete_lifecycle() {
    let (dir, home) = canonical_tempdir();
    let token = create_write_token(&home);

    // Create a daily schedule.
    let create = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args([
            "schedule",
            "create",
            "daily-summary",
            "daily at 9am",
            "--prompt",
            "summarize my day",
            "--token",
            &token,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let create_stdout = String::from_utf8(create).expect("stdout utf8");
    assert!(
        create_stdout.contains("created schedule"),
        "{create_stdout}"
    );

    let id = create_stdout
        .lines()
        .find(|l| l.starts_with("created schedule"))
        .and_then(|l| l.split_whitespace().nth(2))
        .expect("schedule id in stdout")
        .to_string();

    // List should contain the schedule.
    let list = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args(["schedule", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list_stdout = String::from_utf8(list).expect("stdout utf8");
    assert!(list_stdout.contains("daily-summary"), "{list_stdout}");

    // Compute next fire times.
    let next = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args(["schedule", "next", &id, "--count", "3"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let next_stdout = String::from_utf8(next).expect("stdout utf8");
    assert!(next_stdout.contains("next fire times"), "{next_stdout}");

    // Fire requires a token.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args(["schedule", "fire", &id])
        .assert()
        .failure();

    // Fire with a valid token succeeds, admits cost, and records history.
    let fire = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args(["schedule", "fire", &id, "--token", &token])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let fire_stdout = String::from_utf8(fire).expect("stdout utf8");
    assert!(fire_stdout.contains("fired schedule"), "{fire_stdout}");
    assert!(fire_stdout.contains("receipt_id"), "{fire_stdout}");

    let history = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args(["schedule", "history", &id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let history_stdout = String::from_utf8(history).expect("stdout utf8");
    assert!(history_stdout.contains("receipt_id"), "{history_stdout}");

    // Pause, then a fire attempt is refused.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args(["schedule", "pause", &id, "--token", &token])
        .assert()
        .success();
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args(["schedule", "fire", &id, "--token", &token])
        .assert()
        .failure();

    // Resume restores fire-ability.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args(["schedule", "resume", &id, "--token", &token])
        .assert()
        .success();
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args(["schedule", "fire", &id, "--token", &token])
        .assert()
        .success();

    // Delete requires a token too.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args(["schedule", "delete", &id])
        .assert()
        .failure();
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args(["schedule", "delete", &id, "--token", &token])
        .assert()
        .success();

    let schedule_path = dir
        .path()
        .join(".ardur")
        .join("schedules")
        .join(format!("{id}.json"));
    assert!(!schedule_path.exists(), "schedule file should be deleted");
}

#[test]
fn schedule_fire_refused_once_budget_is_exhausted() {
    let (_dir, home) = canonical_tempdir();
    let token = create_write_token(&home);

    let create = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args([
            "schedule",
            "create",
            "tight-budget",
            "every hour",
            "--prompt",
            "ping",
            "--budget-cents",
            "5",
            "--token",
            &token,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let create_stdout = String::from_utf8(create).expect("stdout utf8");
    let id = create_stdout
        .lines()
        .find(|l| l.starts_with("created schedule"))
        .and_then(|l| l.split_whitespace().nth(2))
        .expect("schedule id in stdout")
        .to_string();

    // First fire at cost 5 exhausts the 5-cent budget.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args([
            "schedule",
            "fire",
            &id,
            "--token",
            &token,
            "--cost-cents",
            "5",
        ])
        .assert()
        .success();

    // A second fire is refused: no budget remains.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args([
            "schedule",
            "fire",
            &id,
            "--token",
            &token,
            "--cost-cents",
            "1",
        ])
        .assert()
        .failure();
}
