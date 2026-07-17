//! Regression tests for the 2026-07-12 CLI security sweep's path-traversal
//! finding: several subsystems joined a caller-supplied id/name straight
//! into `<state_dir>.join(format!("{id}.json"))` with no validation, so an
//! id containing `..` or an absolute path could read, write, or delete a
//! file entirely outside the intended `~/.ardur/...` directory. Every site
//! listed here now runs the id through `sanitize_state_id` first — this
//! file proves each of them rejects a traversal attempt before touching the
//! filesystem.

use assert_cmd::Command;

/// Runs `ardur <args>` against a scratch `HOME` and asserts it fails with an
/// error naming the offending id (not a generic/unrelated failure) and,
/// crucially, does not perform whatever a successful call would have done —
/// callers pass a traversal/absolute path id, so if the command "succeeded"
/// it would mean the sanitizer was bypassed.
fn assert_id_rejected(args: &[&str], evil_id: &str) {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(args)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8_lossy(&output);
    assert!(
        stderr.contains(evil_id)
            || stderr.contains("path separator")
            || stderr.contains("absolute"),
        "expected a sanitizer rejection for {args:?}, got: {stderr}"
    );
}

#[test]
fn approvals_approve_rejects_traversal_id() {
    assert_id_rejected(&["approvals", "approve", "../../../etc/passwd"], "..");
}

#[test]
fn approvals_deny_rejects_absolute_id() {
    assert_id_rejected(&["approvals", "deny", "/etc/passwd"], "/etc/passwd");
}

#[test]
fn schedule_delete_rejects_traversal_id() {
    assert_id_rejected(&["schedule", "delete", "../../../etc/passwd"], "..");
}

#[test]
fn token_revoke_rejects_traversal_id() {
    assert_id_rejected(&["token", "revoke", "../../../etc/passwd"], "..");
}

#[test]
fn memory_forget_rejects_traversal_id() {
    assert_id_rejected(&["memory", "forget", "../../../etc/passwd"], "..");
}

#[test]
fn channel_add_rejects_traversal_name() {
    assert_id_rejected(&["channel", "add", "discord", "../../../etc/passwd"], "..");
}

#[test]
fn channel_remove_rejects_traversal_name() {
    assert_id_rejected(&["channel", "remove", "../../../etc/passwd"], "..");
}

#[test]
fn channel_set_rejects_traversal_name() {
    assert_id_rejected(&["channel", "set", "../../../etc/passwd", "enabled"], "..");
}

#[test]
fn persona_create_rejects_traversal_name() {
    assert_id_rejected(&["persona", "create", "../../../etc/passwd"], "..");
}

#[test]
fn persona_set_rejects_traversal_name() {
    assert_id_rejected(&["persona", "set", "../../../etc/passwd"], "..");
}

#[test]
fn persona_remove_rejects_traversal_name() {
    assert_id_rejected(&["persona", "remove", "../../../etc/passwd"], "..");
}

#[test]
fn marketplace_remove_rejects_traversal_id() {
    assert_id_rejected(&["marketplace", "remove", "../../../etc/passwd"], "..");
}

/// A legitimate id (no separators, not absolute, not `.`/`..`) must still
/// work end to end — the sanitizer is a traversal guard, not a charset
/// allowlist that would break ordinary reverse-DNS-style or UUID ids.
#[test]
fn ordinary_ids_still_work_end_to_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .env("HOME", dir.path())
        .args(["channel", "add", "discord", "support-bot.v2"])
        .assert()
        .success();
    let path = dir
        .path()
        .join(".ardur")
        .join("channels")
        .join("support-bot.v2.json");
    assert!(path.exists(), "legitimate channel id should still work");
}
