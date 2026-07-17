//! §1.8 end-to-end tests for `/checkpoint`, `/checkpoints`, and `/rollback`
//! over the offline stub provider — the same fallback path
//! `cli_fused_offline.rs` exercises, so a real cap-token, receipt chain, and
//! durable journal are all in play, just no network call.

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

/// The single session directory name created under `~/.ardur/journals/sessions`.
fn only_session_id(home: &std::path::Path) -> String {
    let sessions_dir = home.join(".ardur/journals/sessions");
    let mut entries: Vec<_> = std::fs::read_dir(&sessions_dir)
        .expect("sessions dir exists")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one session: {entries:?}"
    );
    entries.remove(0)
}

#[test]
fn checkpoint_checkpoints_and_rollback_round_trip() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    // First run: two turns with a checkpoint between them, then list.
    let first = offline_chat()
        .env("HOME", &home_path)
        .write_stdin(
            "hello\n/checkpoint before the second message\nsecond message\n/checkpoints\n/quit\n",
        )
        .output()
        .expect("the chat process runs");
    assert!(first.status.success(), "exit: {:?}", first.status);
    let first_stdout = String::from_utf8_lossy(&first.stdout).into_owned();

    assert!(
        first_stdout.contains("checkpoint") && first_stdout.contains("created:"),
        "checkpoint creation should be confirmed: {first_stdout}"
    );
    assert!(
        first_stdout.contains("before the second message"),
        "the checkpoint label should be echoed back by /checkpoints: {first_stdout}"
    );

    // Pull the checkpoint id straight out of the durable journal rather than
    // scraping stdout formatting — a more robust source of truth for the id
    // this test needs next.
    let session_id = only_session_id(home.path());
    let journal_path = home
        .path()
        .join(".ardur/journals/sessions")
        .join(&session_id)
        .join("journal.jsonl");
    let journal_contents = std::fs::read_to_string(&journal_path).expect("journal readable");
    let checkpoint_line = journal_contents
        .lines()
        .find(|l| l.contains("\"kind\":\"Checkpoint\""))
        .expect("a Checkpoint entry was journaled");
    let checkpoint_json: serde_json::Value =
        serde_json::from_str(checkpoint_line).expect("checkpoint entry is JSON");
    let checkpoint_id = checkpoint_json["checkpoint_id"]
        .as_str()
        .expect("checkpoint_id field")
        .to_string();

    // No Rollback entry yet.
    assert!(
        !journal_contents.contains("\"kind\":\"Rollback\""),
        "no rollback has happened yet"
    );

    // Second run: resume the same session and roll back to the checkpoint.
    let second = offline_chat()
        .env("HOME", &home_path)
        .arg("--session-id")
        .arg(&session_id)
        .write_stdin(format!("/rollback {checkpoint_id}\n/quit\n"))
        .output()
        .expect("the resumed chat process runs");
    assert!(second.status.success(), "exit: {:?}", second.status);
    let second_stdout = String::from_utf8_lossy(&second.stdout).into_owned();
    assert!(
        second_stdout.contains("rolled back to checkpoint"),
        "rollback should be confirmed: {second_stdout}"
    );

    // The journal now carries a Rollback marker naming the checkpoint — and
    // still carries everything from before, append-only.
    let journal_after = std::fs::read_to_string(&journal_path).expect("journal readable");
    assert!(
        journal_after.contains("\"kind\":\"Rollback\""),
        "a Rollback entry should be journaled: {journal_after}"
    );
    assert!(
        journal_after.contains("second message"),
        "the pre-rollback history must remain in the append-only journal"
    );

    // The rollback receipt chained onto the same receipt log turns use.
    let verify = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["receipts", "verify"])
        .env("HOME", &home_path)
        .output()
        .expect("verify receipt chain");
    assert!(
        verify.status.success(),
        "receipt verification failed after checkpoint+rollback: {}",
        String::from_utf8_lossy(&verify.stderr)
    );
}

/// `/rollback` with a malformed or unknown checkpoint id fails cleanly,
/// without corrupting the journal.
#[test]
fn rollback_rejects_an_unknown_checkpoint_id() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = offline_chat()
        .env("HOME", &home_path)
        .write_stdin("hello\n/rollback 00000000-0000-0000-0000-000000000000\n/quit\n")
        .output()
        .expect("the chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("rollback failed") || stdout.to_lowercase().contains("not found"),
        "an unknown checkpoint id should fail cleanly: {stdout}"
    );

    let session_id = only_session_id(home.path());
    let journal_path = home
        .path()
        .join(".ardur/journals/sessions")
        .join(&session_id)
        .join("journal.jsonl");
    let journal_contents = std::fs::read_to_string(&journal_path).expect("journal readable");
    assert!(
        !journal_contents.contains("\"kind\":\"Rollback\""),
        "a rejected rollback must not append a Rollback entry"
    );
}
