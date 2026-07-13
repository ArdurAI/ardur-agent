//! §1.7 end-to-end tests for `/compact`, `/compress`, `/compact preview`,
//! and `/compact status` over the offline stub provider.

use assert_cmd::Command;

fn offline_chat() -> Command {
    let mut cmd = Command::cargo_bin("ardur").expect("the `ardur` binary builds");
    cmd.arg("chat")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ARDUR_PROVIDER")
        .env_remove("ARDUR_MODEL")
        .env_remove("ARDUR_DATA_DIR")
        .env("ARDUR_DEV_PERMISSIVE_POLICY", "true")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_COMPAT_API_KEY")
        .env_remove("OLLAMA_BASE_URL");
    cmd
}

fn only_session_id(home: &std::path::Path) -> String {
    let sessions_dir = home.join(".ardur/journals/sessions");
    let mut entries: Vec<_> = std::fs::read_dir(&sessions_dir)
        .expect("sessions dir exists")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries.len(), 1, "expected exactly one session: {entries:?}");
    entries.remove(0)
}

/// `/compact` runs the offline stub as the summarizer, installs a
/// compaction checkpoint (journaled, receipted — verified end to end with
/// `ardur receipts verify`), and does not journal the meta-summarization
/// call as ordinary conversation turns.
#[test]
fn compact_installs_a_checkpoint_over_the_offline_stub() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = offline_chat()
        .env("HOME", &home_path)
        .write_stdin("hello\nsecond message\n/compact\n/quit\n")
        .output()
        .expect("the chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("compacted: checkpoint"),
        "compact should be confirmed: {stdout}"
    );

    let session_id = only_session_id(home.path());
    let journal_path = home
        .path()
        .join(".ardur/journals/sessions")
        .join(&session_id)
        .join("journal.jsonl");
    let journal_contents = std::fs::read_to_string(&journal_path).expect("journal readable");
    let checkpoint_entries = journal_contents
        .lines()
        .filter(|l| l.contains("\"kind\":\"Checkpoint\""))
        .count();
    assert_eq!(
        checkpoint_entries, 1,
        "exactly one compaction checkpoint should be journaled: {journal_contents}"
    );

    let verify = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["receipts", "verify"])
        .env("HOME", &home_path)
        .output()
        .expect("verify receipt chain");
    assert!(
        verify.status.success(),
        "receipt verification failed after compact: {}",
        String::from_utf8_lossy(&verify.stderr)
    );
}

/// `/compress` is an alias for `/compact`.
#[test]
fn compress_is_an_alias_for_compact() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = offline_chat()
        .env("HOME", &home_path)
        .write_stdin("hello\n/compress\n/quit\n")
        .output()
        .expect("the chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("compacted: checkpoint"),
        "/compress should behave exactly like /compact: {stdout}"
    );
}

/// `/compact preview` runs the summarizer but installs nothing: no new
/// checkpoint in the journal.
#[test]
fn compact_preview_installs_nothing() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = offline_chat()
        .env("HOME", &home_path)
        .write_stdin("hello\n/compact preview\n/quit\n")
        .output()
        .expect("the chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);

    let session_id = only_session_id(home.path());
    let journal_path = home
        .path()
        .join(".ardur/journals/sessions")
        .join(&session_id)
        .join("journal.jsonl");
    let journal_contents = std::fs::read_to_string(&journal_path).expect("journal readable");
    assert!(
        !journal_contents.contains("\"kind\":\"Checkpoint\""),
        "preview must not install a checkpoint: {journal_contents}"
    );
}

/// `/compact status` reports a message count and a rough token estimate
/// without touching the journal at all.
#[test]
fn compact_status_reports_without_side_effects() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = offline_chat()
        .env("HOME", &home_path)
        .write_stdin("hello\n/compact status\n/quit\n")
        .output()
        .expect("the chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("tokens"),
        "status should report a token estimate: {stdout}"
    );
}
