//! ARD-139 end-to-end tests for `ardur approvals list|approve|deny`: the
//! CLI decide-half of the approval-gate loop, now backed by
//! `ardur-approvals`'s shared store instead of ad-hoc `serde_json::Value`
//! pokes, and minting a real signed `approval.approve.accepted.v1`/
//! `approval.reject.accepted.v1` receipt chained onto `~/.ardur/receipts/
//! chain.jsonl`.
//!
//! These tests seed a card directly on disk (standing in for a card a
//! `FusedRuntime`'s propose-half would have written — see
//! `crates/fused-runtime/tests/approval_gate.rs` for that half of the
//! loop), then drive the CLI's decide side end to end.

use assert_cmd::Command;
use serde_json::json;

fn ardur() -> Command {
    Command::cargo_bin("ardur").expect("the `ardur` binary builds")
}

/// Write a `Pending` card at `<home>/.ardur/approvals/<id>.json`, matching
/// `ardur_approvals::ApprovalCard`'s on-disk shape.
fn seed_pending_card(home: &std::path::Path, id: &str) {
    let dir = home.join(".ardur").join("approvals");
    std::fs::create_dir_all(&dir).expect("approvals dir creates");
    let card = json!({
        "status": "pending",
        "created_at": 1000,
        "tool": "gated.shell",
        "capability": "cap.shell_exec",
        "arguments_digest": "deadbeef",
        "reason": "tool `gated.shell` requires capability `cap.shell_exec`, which is approval-gated",
    });
    std::fs::write(
        dir.join(format!("{id}.json")),
        serde_json::to_vec_pretty(&card).unwrap(),
    )
    .expect("card writes");
}

fn read_card(home: &std::path::Path, id: &str) -> serde_json::Value {
    let path = home
        .join(".ardur")
        .join("approvals")
        .join(format!("{id}.json"));
    let bytes = std::fs::read(path).expect("card exists");
    serde_json::from_slice(&bytes).expect("card is valid json")
}

fn chain_verbs(home: &std::path::Path) -> Vec<String> {
    let path = home.join(".ardur").join("receipts").join("chain.jsonl");
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut parts = line.split('.');
            let payload_b64 = parts.nth(1).expect("jws has a payload segment");
            use base64::Engine as _;
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(payload_b64)
                .expect("payload decodes");
            let body: serde_json::Value =
                serde_json::from_slice(&payload).expect("payload is json");
            body["verb"].as_str().expect("verb is a string").to_string()
        })
        .collect()
}

/// `ardur approvals list` reports a seeded pending card.
#[test]
fn list_reports_a_seeded_pending_card() {
    let home_tmp = tempfile::tempdir().expect("temp HOME");
    let home = home_tmp.path().canonicalize().expect("canonical temp HOME");
    seed_pending_card(&home, "card-1");

    let output = ardur()
        .env("HOME", &home)
        .args(["approvals", "list"])
        .output()
        .expect("the approvals process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("card-1"), "{stdout}");
    assert!(stdout.contains("gated.shell"), "{stdout}");
}

/// `ardur approvals list` with nothing pending says so cleanly.
#[test]
fn list_with_nothing_pending_reports_cleanly() {
    let home_tmp = tempfile::tempdir().expect("temp HOME");
    let home = home_tmp.path().canonicalize().expect("canonical temp HOME");

    let output = ardur()
        .env("HOME", &home)
        .args(["approvals", "list"])
        .output()
        .expect("the approvals process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("no pending approvals"), "{stdout}");
}

/// `ardur approvals approve <id>` flips the card to `approved`, stamps
/// `decided_at`, and mints a real `approval.approve.accepted.v1` receipt
/// chained onto `~/.ardur/receipts/chain.jsonl`.
#[test]
fn approve_flips_status_and_mints_a_signed_receipt() {
    let home_tmp = tempfile::tempdir().expect("temp HOME");
    let home = home_tmp.path().canonicalize().expect("canonical temp HOME");
    seed_pending_card(&home, "card-1");

    let output = ardur()
        .env("HOME", &home)
        .args(["approvals", "approve", "card-1"])
        .output()
        .expect("the approvals process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("approved card-1"), "{stdout}");

    let card = read_card(&home, "card-1");
    assert_eq!(card["status"], "approved");
    assert!(card["decided_at"].is_number());

    let verbs = chain_verbs(&home);
    assert_eq!(verbs, vec!["approval.approve.accepted.v1"]);
}

/// `ardur approvals deny <id> --reason ...` flips the card to `denied`,
/// records the reason, and mints a real `approval.reject.accepted.v1`
/// receipt.
#[test]
fn deny_records_the_reason_and_mints_a_signed_receipt() {
    let home_tmp = tempfile::tempdir().expect("temp HOME");
    let home = home_tmp.path().canonicalize().expect("canonical temp HOME");
    seed_pending_card(&home, "card-1");

    let output = ardur()
        .env("HOME", &home)
        .args(["approvals", "deny", "card-1", "--reason", "too risky"])
        .output()
        .expect("the approvals process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("denied card-1"), "{stdout}");

    let card = read_card(&home, "card-1");
    assert_eq!(card["status"], "denied");
    assert_eq!(card["deny_reason"], "too risky");

    let verbs = chain_verbs(&home);
    assert_eq!(verbs, vec!["approval.reject.accepted.v1"]);
}

/// A second decision against an already-decided card fails cleanly (exit
/// non-zero, no second receipt), rather than silently overwriting.
#[test]
fn deciding_twice_is_rejected_not_silently_overwritten() {
    let home_tmp = tempfile::tempdir().expect("temp HOME");
    let home = home_tmp.path().canonicalize().expect("canonical temp HOME");
    seed_pending_card(&home, "card-1");

    ardur()
        .env("HOME", &home)
        .args(["approvals", "approve", "card-1"])
        .assert()
        .success();

    let second = ardur()
        .env("HOME", &home)
        .args(["approvals", "approve", "card-1"])
        .output()
        .expect("the approvals process runs");
    assert!(
        !second.status.success(),
        "a second decision against an already-decided card must fail"
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("already decided"), "{stderr}");

    assert_eq!(
        chain_verbs(&home).len(),
        1,
        "the rejected second attempt must not mint a second receipt"
    );
}

/// Deciding an unknown id fails cleanly rather than crashing.
#[test]
fn deciding_an_unknown_id_fails_cleanly() {
    let home_tmp = tempfile::tempdir().expect("temp HOME");
    let home = home_tmp.path().canonicalize().expect("canonical temp HOME");

    let output = ardur()
        .env("HOME", &home)
        .args(["approvals", "approve", "does-not-exist"])
        .output()
        .expect("the approvals process runs");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not found"), "{stderr}");
}
