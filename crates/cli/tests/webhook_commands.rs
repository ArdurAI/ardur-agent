//! Integration tests for `ardur webhook` (§9.7 inbound trigger surface).

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

fn create_schedule(home: &std::path::Path, token: &str) -> String {
    let output = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", home)
        .args([
            "schedule",
            "create",
            "webhook-bound",
            "every hour",
            "--prompt",
            "handle the webhook event",
            "--token",
            token,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).expect("stdout utf8");
    stdout
        .lines()
        .find(|l| l.starts_with("created schedule"))
        .and_then(|l| l.split_whitespace().nth(2))
        .expect("schedule id in stdout")
        .to_string()
}

#[test]
fn webhook_add_requires_an_existing_schedule() {
    let (_dir, home) = canonical_tempdir();
    let token = create_write_token(&home);

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args([
            "webhook",
            "add",
            "github",
            "--schedule-id",
            "does-not-exist",
            "--secret",
            "hook-secret",
            "--token",
            &token,
        ])
        .assert()
        .failure();
}

#[test]
fn webhook_add_list_remove_lifecycle() {
    let (_dir, home) = canonical_tempdir();
    let token = create_write_token(&home);
    let schedule_id = create_schedule(&home, &token);

    // Registering without a token is refused.
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args([
            "webhook",
            "add",
            "github",
            "--schedule-id",
            &schedule_id,
            "--secret",
            "hook-secret",
        ])
        .assert()
        .failure();

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args([
            "webhook",
            "add",
            "github",
            "--schedule-id",
            &schedule_id,
            "--secret",
            "hook-secret",
            "--token",
            &token,
        ])
        .assert()
        .success();

    let list = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args(["webhook", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list_stdout = String::from_utf8(list).expect("stdout utf8");
    assert!(list_stdout.contains("github"), "{list_stdout}");
    assert!(
        !list_stdout.contains("hook-secret"),
        "the secret must never be printed: {list_stdout}"
    );

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args(["webhook", "remove", "github", "--token", &token])
        .assert()
        .success();

    let list_after = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args(["webhook", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let list_after_stdout = String::from_utf8(list_after).expect("stdout utf8");
    assert!(!list_after_stdout.contains("github"), "{list_after_stdout}");
}

#[test]
fn webhook_test_verifies_signature_and_fires_bound_schedule() {
    let (_dir, home) = canonical_tempdir();
    let token = create_write_token(&home);
    let schedule_id = create_schedule(&home, &token);

    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args([
            "webhook",
            "add",
            "github",
            "--schedule-id",
            &schedule_id,
            "--secret",
            "hook-secret",
            "--token",
            &token,
        ])
        .assert()
        .success();

    let test_output = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args(["webhook", "test", "github"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(test_output).expect("stdout utf8");
    assert!(stdout.contains("signature verify: ok"), "{stdout}");
    assert!(stdout.contains("fired schedule"), "{stdout}");

    let history = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", &home)
        .args(["schedule", "history", &schedule_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let history_stdout = String::from_utf8(history).expect("stdout utf8");
    assert!(
        history_stdout.contains("webhook-test:github"),
        "{history_stdout}"
    );
}
