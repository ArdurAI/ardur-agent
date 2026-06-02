//! With no `ANTHROPIC_API_KEY`, the default `ardur chat` path falls back to the
//! network-free stub provider, prints an offline notice, and still drives a turn
//! through the full FusedRuntime pipeline (cap-token → Cedar → cost → provider →
//! receipt → finalize → memory → journal), printing the stub's response.

use assert_cmd::Command;

#[test]
fn offline_mode_runs_a_turn_through_the_fused_stub() {
    let home = tempfile::tempdir().expect("temp HOME");

    let output = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .arg("chat")
        .env("HOME", home.path())
        // Force the offline fallback regardless of the ambient environment.
        .env_remove("ANTHROPIC_API_KEY")
        .write_stdin("hello fused substrate\n/quit\n")
        .output()
        .expect("the chat process runs");

    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // The offline notice surfaced.
    assert!(
        stdout.contains("offline mode"),
        "offline notice should be printed, got: {stdout}"
    );
    // The turn ran end-to-end through the fused runtime and printed the stub's
    // deterministic completion (the provider WAS dispatched — not an echo).
    assert!(
        stdout.contains("[anthropic stub]"),
        "the stub provider's completion should be printed, got: {stdout}"
    );

    // A turn through the fused pipeline persists state: the receipt chain and a
    // session journal must now exist on disk.
    let receipts = home.path().join(".ardur/receipts/chain.jsonl");
    assert!(
        receipts.exists(),
        "the signed-receipt chain should be persisted at {}",
        receipts.display()
    );
    let journals = home.path().join(".ardur/journals/sessions");
    assert!(
        journals.exists(),
        "a session journal directory should exist under {}",
        journals.display()
    );
}
