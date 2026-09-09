//! #408 — first-run policy UX.
//!
//! 1. `ardur setup --yes` writes a scoped starter Cedar policy, after which the
//!    offline fused chat path works WITHOUT the `ARDUR_DEV_PERMISSIVE_POLICY`
//!    escape hatch.
//! 2. setup never overwrites an operator's existing `cedar.policies`.
//! 3. A policy denial on an install without a policy file prints an actionable
//!    hint, not just the bare Cedar reason.

use assert_cmd::Command;

/// Build an `ardur` invocation hermetic from the ambient shell: temp HOME, no
/// provider keys, and — critically for this test — NO dev-permissive flag
/// leaking in from a developer's environment.
fn ardur(home: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("ardur").expect("the `ardur` binary builds");
    cmd.env("HOME", home)
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ARDUR_PROVIDER")
        .env_remove("ARDUR_MODEL")
        .env_remove("ARDUR_DATA_DIR")
        .env_remove("ARDUR_DEV_PERMISSIVE_POLICY")
        .env_remove("ARDUR_CLI_BUDGET_CENTS")
        .env_remove("ARDUR_CLI_PER_TURN_CENTS")
        .env_remove("ARDUR_CEDAR_POLICY_PATH")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_COMPAT_API_KEY")
        .env_remove("OLLAMA_BASE_URL");
    cmd
}

#[test]
fn setup_writes_starter_policy_and_offline_chat_works_without_dev_flag() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let setup = ardur(&home_path)
        .args(["setup", "--yes"])
        .output()
        .expect("setup runs");
    assert!(
        setup.status.success(),
        "setup failed: {}",
        String::from_utf8_lossy(&setup.stderr)
    );
    let stdout = String::from_utf8_lossy(&setup.stdout);
    assert!(
        stdout.contains("created starter Cedar policy"),
        "setup should report the starter policy, got: {stdout}"
    );

    let policy_path = home_path.join(".ardur/cedar.policies");
    let policy = std::fs::read_to_string(&policy_path).expect("starter policy written");
    assert!(
        policy.contains("Action::\"Submit\"") && policy.contains("Action::\"ToolInvoke\""),
        "starter policy should permit Submit + ToolInvoke, got: {policy}"
    );
    assert!(
        !policy.contains("permit(principal, action, resource);"),
        "starter policy must NOT be the permit-all dev fallback"
    );

    // The acceptance criterion: a fresh install chats offline with NO dev flag.
    let chat = ardur(&home_path)
        .arg("chat")
        .write_stdin("hello first run\n/quit\n")
        .output()
        .expect("chat runs");
    assert!(chat.status.success(), "exit: {:?}", chat.status);
    let stdout = String::from_utf8_lossy(&chat.stdout);
    assert!(
        stdout.contains("[anthropic stub]"),
        "the fused stub turn should complete after setup, got stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&chat.stderr)
    );

    // Re-running setup is idempotent: the starter is kept, never replaced
    // (codex review: create-new-only semantics, no replacement race).
    let again = ardur(&home_path)
        .args(["setup", "--yes"])
        .output()
        .expect("second setup runs");
    assert!(again.status.success());
    assert!(
        String::from_utf8_lossy(&again.stdout).contains("kept existing Cedar policy"),
        "second setup must keep the existing policy, got: {}",
        String::from_utf8_lossy(&again.stdout)
    );
    assert_eq!(
        std::fs::read_to_string(&policy_path).expect("policy file"),
        policy,
        "starter policy content must be unchanged after a second setup"
    );
}

#[test]
fn setup_never_overwrites_an_existing_policy_file() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");
    let ardur_dir = home_path.join(".ardur");
    std::fs::create_dir_all(&ardur_dir).expect("state dir");
    let policy_path = ardur_dir.join("cedar.policies");
    let operator_policy = "// operator-owned custom policy\nforbid(principal, action, resource);\n";
    std::fs::write(&policy_path, operator_policy).expect("seed operator policy");

    let setup = ardur(&home_path)
        .args(["setup", "--yes"])
        .output()
        .expect("setup runs");
    assert!(setup.status.success());
    let stdout = String::from_utf8_lossy(&setup.stdout);
    assert!(
        stdout.contains("kept existing Cedar policy"),
        "setup should report keeping the operator policy, got: {stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(&policy_path).expect("policy file"),
        operator_policy,
        "operator policy content must be byte-identical after setup"
    );
}

#[test]
fn policy_denial_without_policy_file_prints_actionable_hint() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    // No setup: the fail-closed default must fire AND explain itself.
    let chat = ardur(&home_path)
        .arg("chat")
        .write_stdin("hello denied\n/quit\n")
        .output()
        .expect("chat runs");
    let stderr = String::from_utf8_lossy(&chat.stderr);
    assert!(
        stderr.contains("policy denied"),
        "the denial reason should surface, got stderr: {stderr}"
    );
    assert!(
        stderr.contains("hint: no Cedar policy file"),
        "the actionable hint should follow the denial, got stderr: {stderr}"
    );
    assert!(
        stderr.contains("ardur setup"),
        "the hint should name the remedy command, got stderr: {stderr}"
    );
}
