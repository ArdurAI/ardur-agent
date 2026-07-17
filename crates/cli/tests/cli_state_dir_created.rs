//! The default `ardur chat` path materializes the persistent state tree under
//! `~/.ardur/` on first run: `memory/`, `journals/`, `receipts/`, and `keys/`,
//! and mints the issuer + receipt keys.

use assert_cmd::Command;

#[test]
fn first_run_creates_the_state_directories_and_keys() {
    let home = tempfile::tempdir().expect("temp HOME");
    let ardur = home.path().join(".ardur");

    // Nothing exists yet.
    assert!(!ardur.exists(), "the state root should not pre-exist");

    // Drive an immediate EOF (empty stdin) — the engine wires (creating the
    // state tree) before the loop reads its first line.
    let output = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .arg("chat")
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .env_remove("ANTHROPIC_API_KEY")
        .write_stdin("")
        .output()
        .expect("the chat process runs");

    assert!(output.status.success(), "exit: {:?}", output.status);

    for sub in ["memory", "journals", "receipts", "keys"] {
        let dir = ardur.join(sub);
        assert!(
            dir.is_dir(),
            "first run should create ~/.ardur/{sub} at {}",
            dir.display()
        );
    }
    // The persistent keys were minted on first run.
    assert!(
        ardur.join("keys/issuer.key").exists(),
        "the cap-token issuer key should be persisted"
    );
    assert!(
        ardur.join("keys/receipt.pem").exists(),
        "the receipt signing key should be persisted"
    );
}
