//! The default `ardur chat` path honours the `ARDUR_PROVIDER` selector: it wires
//! the chosen backend (not the hard-coded Anthropic stub) and logs the selection
//! at startup.
//!
//! These drive the real `ardur` binary with a child-process environment (the
//! established pattern — no in-process env mutation). They quit immediately
//! after boot, so no turn runs and the credential-free backends (`ollama`,
//! `codex`) are never actually contacted over the network.

use assert_cmd::Command;

/// Boot `ardur chat` with `ARDUR_PROVIDER=<selector>` and no Anthropic key,
/// quit, and return `(stdout, stderr)`.
fn boot_with_selector(selector: &str) -> (String, String) {
    let home = tempfile::tempdir().expect("temp HOME");
    let output = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .arg("chat")
        .env("HOME", home.path())
        .env("ARDUR_PROVIDER", selector)
        // Strip the ambient key so the *only* reason a real backend is wired is
        // the selector — never a fallback to the offline Anthropic stub.
        .env_remove("ANTHROPIC_API_KEY")
        .write_stdin("/quit\n")
        .output()
        .expect("the chat process runs");

    assert!(output.status.success(), "exit: {:?}", output.status);
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn cli_uses_selected_ollama_provider() {
    let (stdout, stderr) = boot_with_selector("ollama");

    // The startup log names the selected provider.
    assert!(
        stderr.contains("using provider") && stderr.contains("ollama"),
        "startup log should report the ollama selection, got stderr: {stderr}"
    );
    // A credential-free backend was wired, so the session is NOT offline — the
    // Anthropic-stub offline notice must be absent.
    assert!(
        !stdout.contains("offline mode"),
        "ollama is not the offline stub, yet an offline notice appeared: {stdout}"
    );
}

#[test]
fn cli_uses_selected_codex_provider() {
    let (stdout, stderr) = boot_with_selector("codex");

    assert!(
        stderr.contains("using provider") && stderr.contains("codex"),
        "startup log should report the codex selection, got stderr: {stderr}"
    );
    assert!(
        !stdout.contains("offline mode"),
        "codex is not the offline stub, yet an offline notice appeared: {stdout}"
    );
}

#[test]
fn cli_case_insensitive_selector() {
    // Mixed case still resolves — proving the parse is case-folding, not literal.
    let (_stdout, stderr) = boot_with_selector("OLLAMA");
    assert!(
        stderr.contains("using provider") && stderr.contains("ollama"),
        "an upper-case selector should still wire ollama, got stderr: {stderr}"
    );
}
