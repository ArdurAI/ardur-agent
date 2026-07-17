//! `ardur chat --echo` preserves the legacy in-memory echo behavior: a turn
//! piped on stdin is echoed back, with no provider, cost, or persistent state.

use assert_cmd::Command;

#[test]
fn echo_mode_echoes_a_piped_turn() {
    // A fresh HOME proves `--echo` never touches `~/.ardur/`: even with this
    // empty state root the echo path needs no keys, policies, or journals.
    let home = tempfile::tempdir().expect("temp HOME");

    let output = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .arg("chat")
        .arg("--echo")
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .env_remove("ANTHROPIC_API_KEY")
        .write_stdin("hello echo world\n/quit\n")
        .output()
        .expect("the chat process runs");

    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The InMemoryRuntime echoes the user's message back verbatim.
    assert!(
        stdout.contains("hello echo world"),
        "echo output should contain the prompt, got: {stdout}"
    );

    // No state directory was created by the echo path.
    assert!(
        !home.path().join(".ardur").exists(),
        "--echo must not create the ~/.ardur state tree"
    );
}
