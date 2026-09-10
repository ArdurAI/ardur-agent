//! ARD-457 — `ardur grant` round-trip: recording an operator grant appends to
//! the `~/.ardur/grants.json` ledger *and* chains a signed `tool.grant.allow.v1`
//! receipt that keeps `ardur receipts verify` green.

use assert_cmd::Command;

/// A canonical tempdir path for use as `HOME`. Canonicalizing resolves the
/// `/var` → `/private/var` (macOS) symlink so the CLI's no-follow state I/O —
/// which refuses to traverse symlinked path components — can open the tree.
fn canonical_home() -> (tempfile::TempDir, std::path::PathBuf) {
    let home = tempfile::tempdir().expect("tempdir");
    let path = home.path().canonicalize().expect("canonical temp HOME");
    (home, path)
}

/// Point the CLI's `~/.ardur/` resolution at `home` so the test never touches
/// the real state directory.
fn ardur(home: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("ardur").expect("the `ardur` binary builds");
    cmd.env("HOME", home).env_remove("USERPROFILE");
    cmd
}

#[test]
fn grant_allow_records_ledger_and_chains_a_verifiable_receipt() {
    let (_home_guard, home) = canonical_home();

    // Two grants, so the second must chain onto the first's receipt.
    let first = ardur(&home)
        .args(["grant", "allow", "shell.run", "--scope", "git|cargo"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let first = String::from_utf8(first).expect("stdout utf8");
    assert!(
        first.contains("granted `shell.run`") && first.contains("cap.shell_exec"),
        "grant output should name the tool and its capabilities: {first}"
    );

    ardur(&home)
        .args(["grant", "allow", "http.fetch"])
        .assert()
        .success();

    // The ledger holds both grants with their capabilities and a receipt id.
    let listed = ardur(&home)
        .args(["grant", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let listed = String::from_utf8(listed).expect("stdout utf8");
    let grants: serde_json::Value = serde_json::from_str(&listed).expect("grant list is JSON");
    let grants = grants.as_array().expect("grant list is an array");
    assert_eq!(grants.len(), 2, "both grants are recorded: {listed}");
    assert_eq!(grants[0]["tool"], "shell.run");
    assert_eq!(grants[0]["scope"], "git|cargo");
    assert_eq!(grants[0]["capabilities"][0], "cap.shell_exec");
    assert_eq!(grants[1]["tool"], "http.fetch");
    assert!(
        grants[0]["receipt_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "each grant carries its receipt id: {listed}"
    );

    // The grant receipts chained correctly and are signed by the same key the
    // chat runtime uses — so the full-chain verifier accepts them.
    let verified = ardur(&home)
        .args(["receipts", "verify"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let verified = String::from_utf8(verified).expect("stdout utf8");
    assert!(
        verified.contains("authenticated complete chain of 2 receipts"),
        "the two grant receipts verify as a complete chain: {verified}"
    );

    // And the receipts surface under the grant verb.
    let receipts = ardur(&home)
        .args(["receipts", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let receipts = String::from_utf8(receipts).expect("stdout utf8");
    assert!(
        receipts.contains("tool.grant.allow.v1"),
        "receipt chain shows the grant verb: {receipts}"
    );
}

#[test]
fn grant_allow_rejects_an_unknown_tool() {
    let (_home_guard, home) = canonical_home();
    let stderr = ardur(&home)
        .args(["grant", "allow", "rm.rf"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(stderr).expect("stderr utf8");
    assert!(
        stderr.contains("unknown tool `rm.rf`"),
        "unknown tools are rejected: {stderr}"
    );
}

#[test]
fn grant_list_is_empty_before_any_grant() {
    let (_home_guard, home) = canonical_home();
    let stdout = ardur(&home)
        .args(["grant", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(stdout).expect("stdout utf8");
    assert!(
        stdout.contains("no grants recorded"),
        "an empty ledger reports no grants: {stdout}"
    );
}
