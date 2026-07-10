//! With no `ANTHROPIC_API_KEY`, the default `ardur chat` path falls back to the
//! network-free stub provider, prints an offline notice, and still drives a turn
//! through the full FusedRuntime pipeline (cap-token → Cedar → cost → provider →
//! receipt → finalize → memory → journal), printing the stub's response.

use assert_cmd::Command;

#[test]
fn offline_mode_runs_a_turn_through_the_fused_stub() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = Command::cargo_bin("ardur")
        .expect("the `ardur` binary builds")
        .arg("chat")
        .env("HOME", &home_path)
        // Force the offline fallback regardless of the ambient environment.
        .env_remove("ANTHROPIC_API_KEY")
        // Isolate from ambient provider/model env vars that would bypass the
        // Anthropic stub (e.g. ARDUR_PROVIDER=ollama from a dev shell).
        .env_remove("ARDUR_PROVIDER")
        .env_remove("ARDUR_MODEL")
        .env_remove("ARDUR_DATA_DIR")
        // Missing CLI Cedar policy is fail-closed by default; this smoke test is
        // intentionally the local-dev path over the offline provider.
        .env("ARDUR_DEV_PERMISSIVE_POLICY", "true")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_COMPAT_API_KEY")
        .env_remove("OLLAMA_BASE_URL")
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

    let verify = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["receipts", "verify"])
        .env("HOME", &home_path)
        .output()
        .expect("verify receipt chain");
    assert!(
        verify.status.success(),
        "receipt verification failed: {}",
        String::from_utf8_lossy(&verify.stderr)
    );
    assert!(
        String::from_utf8_lossy(&verify.stdout).contains("ES256 signatures OK"),
        "verification must report authenticated signatures"
    );

    let list = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["receipts", "list"])
        .env("HOME", &home_path)
        .output()
        .expect("list receipts");
    assert!(list.status.success(), "receipt list failed");
    let listed: serde_json::Value =
        serde_json::from_slice(&list.stdout).expect("receipt list is JSON");
    let receipt_id = listed[0]["receipt_id"].as_str().expect("listed receipt id");
    let session_id = listed[0]["session_id"]
        .as_str()
        .expect("receipt is scoped to its durable journal");
    assert!(
        listed[0]["jws_compact"].as_str().is_some(),
        "receipt evidence includes the authenticated compact JWS"
    );

    let session_prefix = &session_id[..8];
    let filtered = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["receipts", "list", "--session", session_prefix])
        .env("HOME", &home_path)
        .output()
        .expect("filter receipts by session");
    assert!(filtered.status.success(), "session receipt filter failed");
    let filtered_json: serde_json::Value =
        serde_json::from_slice(&filtered.stdout).expect("filtered receipt list is JSON");
    assert_eq!(filtered_json[0]["session_id"], session_id);

    let other_session = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["receipts", "list", "--session", "ffffffff"])
        .env("HOME", &home_path)
        .output()
        .expect("filter receipts for unrelated session");
    assert!(other_session.status.success());
    assert_eq!(
        String::from_utf8_lossy(&other_session.stdout).trim(),
        "no receipts found"
    );

    let show = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["receipts", "show", receipt_id])
        .env("HOME", &home_path)
        .output()
        .expect("show receipt");
    assert!(show.status.success(), "receipt show failed");
    let shown: serde_json::Value =
        serde_json::from_slice(&show.stdout).expect("shown receipt is JSON");
    assert_eq!(shown["receipt_id"], receipt_id);

    let mut tampered = std::fs::read_to_string(&receipts).expect("receipt chain");
    let signature_start = tampered.rfind('.').expect("JWS signature segment") + 1;
    let original = tampered.as_bytes()[signature_start] as char;
    tampered.replace_range(
        signature_start..signature_start + 1,
        if original == 'A' { "B" } else { "A" },
    );
    std::fs::write(&receipts, tampered).expect("tamper receipt signature");
    let forged = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["receipts", "verify"])
        .env("HOME", &home_path)
        .output()
        .expect("verify forged receipt chain");
    assert!(
        !forged.status.success(),
        "forged receipt signature must fail verification"
    );
}
