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
    // caps) and completes a fused stub turn without error.
    let chat = ardur(&home_path)
        .arg("chat")
        .write_stdin("hello granted substrate\n/quit\n")
        .output()
        .expect("chat runs");
    assert!(chat.status.success(), "exit: {:?}", chat.status);
    let stdout = String::from_utf8_lossy(&chat.stdout);
    assert!(
        stdout.contains("[anthropic stub]"),
        "the fused stub turn should complete with a grant present, got: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&chat.stderr)
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
