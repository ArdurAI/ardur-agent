//! ARD-457 — the operator grant ledger is consumed by the CLI chat engine.
//!
//! Binary-level loop: setup → grant a tool → offline chat turn → the receipt
//! chain (grant receipt + turn receipt) verifies end to end. The capability
//! mapping itself is unit-tested in `crates/cli/src/fused.rs::grant_tooling_tests`.

use assert_cmd::Command;

fn ardur(home: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("ardur").expect("the `ardur` binary builds");
    cmd.env("HOME", home)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ARDUR_PROVIDER")
        .env_remove("ARDUR_MODEL")
        .env_remove("ARDUR_DATA_DIR")
        .env_remove("ARDUR_DEV_PERMISSIVE_POLICY")
        // Budget env from the developer shell must not leak into the turn's
        // cost admission.
        .env_remove("ARDUR_CLI_BUDGET_CENTS")
        .env_remove("ARDUR_CLI_PER_TURN_CENTS")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_COMPAT_API_KEY")
        .env_remove("OLLAMA_BASE_URL");
    cmd
}

#[test]
fn granted_chat_turn_runs_and_the_combined_chain_verifies() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let setup = ardur(&home_path)
        .args(["setup", "--yes"])
        .output()
        .expect("setup runs");
    assert!(setup.status.success());

    let grant = ardur(&home_path)
        .args(["grant", "allow", "shell.run", "--scope", "echo"])
        .output()
        .expect("grant runs");
    assert!(
        grant.status.success(),
        "grant failed: {}",
        String::from_utf8_lossy(&grant.stderr)
    );
    assert!(home_path.join(".ardur/grants.json").exists());

    // The chat turn boots over the grant ledger (registers shell.run, mints its
    // caps) and completes a fused stub turn without error. The registration is
    // OBSERVABLE: the engine logs each granted tool it activates, so a no-op
    // consumer fails this assertion.
    let chat = ardur(&home_path)
        .arg("chat")
        .write_stdin("hello granted substrate\n/quit\n")
        .output()
        .expect("chat runs");
    assert!(chat.status.success(), "exit: {:?}", chat.status);
    let stdout = String::from_utf8_lossy(&chat.stdout);
    let stderr = String::from_utf8_lossy(&chat.stderr);
    assert!(
        stdout.contains("[anthropic stub]"),
        "the fused stub turn should complete with a grant present, got: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stderr.contains("registered operator-granted tool") && stderr.contains("shell.run"),
        "the granted tool's registration log must appear, got stderr: {stderr}"
    );

    // Grant receipt + turn receipt live in one chain; it must verify.
    let verify = ardur(&home_path)
        .args(["receipts", "verify"])
        .output()
        .expect("verify runs");
    assert!(
        verify.status.success(),
        "chain verification failed: {}",
        String::from_utf8_lossy(&verify.stderr)
    );
    assert!(
        String::from_utf8_lossy(&verify.stdout).contains("ES256 signatures OK"),
        "verification must report authenticated signatures"
    );
}

#[test]
fn grant_without_scope_prints_the_consumption_note_for_shell() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let grant = ardur(&home_path)
        .args(["grant", "allow", "shell.run"])
        .output()
        .expect("grant runs");
    assert!(grant.status.success());
    let stdout = String::from_utf8_lossy(&grant.stdout);
    assert!(
        stdout.contains("need --scope"),
        "a scope-less shell grant must print the consumption note, got: {stdout}"
    );
}

#[test]
fn file_grants_record_the_canonical_absolute_scope() {
    // ARD-457 review: `--scope .` must not mean "whatever directory chat later
    // starts in" — the durable ledger records the canonical absolute path
    // resolved at grant time, and a scope that does not resolve is rejected.
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");
    let scope_dir = tempfile::tempdir().expect("scope dir");

    let grant = ardur(&home_path)
        .args(["grant", "allow", "file.read", "--scope", "."])
        .current_dir(scope_dir.path())
        .output()
        .expect("grant runs");
    assert!(
        grant.status.success(),
        "grant failed: {}",
        String::from_utf8_lossy(&grant.stderr)
    );

    let ledger: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home_path.join(".ardur/grants.json")).expect("ledger"),
    )
    .expect("grant ledger parses");
    let recorded = ledger[0]["scope"].as_str().expect("scope recorded");
    assert_eq!(
        recorded,
        std::fs::canonicalize(scope_dir.path())
            .expect("canonical")
            .display()
            .to_string(),
        "a relative scope must be recorded as the canonical absolute path"
    );
    assert!(std::path::Path::new(recorded).is_absolute());

    // A scope that does not resolve to an existing directory is rejected.
    let bad = ardur(&home_path)
        .args([
            "grant",
            "allow",
            "file.read",
            "--scope",
            "./definitely-not-here",
        ])
        .current_dir(scope_dir.path())
        .output()
        .expect("grant runs");
    assert!(
        !bad.status.success(),
        "a nonexistent scope must be rejected"
    );
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("does not resolve"),
        "rejection must explain the resolution failure: {}",
        String::from_utf8_lossy(&bad.stderr)
    );
}
