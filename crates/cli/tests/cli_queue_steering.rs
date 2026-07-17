//! §1.10 end-to-end tests for `/steer` (+ its `/tell` alias), `/interrupt`,
//! and `/queue` (+ its `/status` alias) over the offline stub provider.
//!
//! As with `cli_background_task.rs`, a background task's id is random and
//! only known once already printed, so a static piped-stdin script can't
//! target a real just-started task from a later line. These tests prove
//! the command surface end to end (usage errors, malformed ids, "not
//! found" handling, aliasing, and the queue summary's shape); the real
//! receipt-minting behavior of `accept_steer_directive`/`accept_interrupt`
//! is covered directly in `crates/fused-runtime/tests/queue_steering.rs`.

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

/// `/steer` and `/interrupt` on an unknown id both report cleanly rather
/// than crashing.
#[test]
fn steer_and_interrupt_report_cleanly_for_an_unknown_id() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = offline_chat()
        .env("HOME", &home_path)
        .write_stdin(
            "/steer 00000000-0000-0000-0000-000000000000 narrow the scope\n\
             /interrupt 00000000-0000-0000-0000-000000000000\n\
             /quit\n",
        )
        .output()
        .expect("the chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let not_found_count = stdout.matches("not found").count();
    assert_eq!(
        not_found_count, 2,
        "steer and interrupt should each report the unknown id cleanly: {stdout}"
    );
}

/// `/tell` is an alias for `/steer`.
#[test]
fn tell_is_an_alias_for_steer() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = offline_chat()
        .env("HOME", &home_path)
        .write_stdin(
            "/tell 00000000-0000-0000-0000-000000000000 keep going\n\
             /quit\n",
        )
        .output()
        .expect("the chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("not found"),
        "/tell should route through the same steer handling: {stdout}"
    );
}

/// A malformed (non-UUID) task id is a usage error, not a parse panic, for
/// both `/steer` and `/interrupt`.
#[test]
fn steer_and_interrupt_reject_a_malformed_id() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = offline_chat()
        .env("HOME", &home_path)
        .write_stdin("/steer not-a-uuid hello\n/interrupt not-a-uuid\n/quit\n")
        .output()
        .expect("the chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let usage_count = stdout.matches("usage:").count();
    assert_eq!(usage_count, 2, "{stdout}");
}

/// `/steer <id>` with no message is a usage error, not a steer with an
/// empty message.
#[test]
fn steer_with_no_message_is_a_usage_error() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = offline_chat()
        .env("HOME", &home_path)
        .write_stdin("/steer 00000000-0000-0000-0000-000000000000\n/quit\n")
        .output()
        .expect("the chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("usage:"), "{stdout}");
}

/// `/queue` on an empty registry reports a zero-active, zero-terminal,
/// zero-pending summary rather than erroring.
#[test]
fn queue_with_no_tasks_reports_a_zeroed_summary() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = offline_chat()
        .env("HOME", &home_path)
        .write_stdin("/queue\n/quit\n")
        .output()
        .expect("the chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("0 active, 0 terminal, 0 steering directive(s) pending delivery"),
        "{stdout}"
    );
}

/// `/status` is an alias for `/queue`.
#[test]
fn status_is_an_alias_for_queue() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = offline_chat()
        .env("HOME", &home_path)
        .write_stdin("/status\n/quit\n")
        .output()
        .expect("the chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("0 active, 0 terminal, 0 steering directive(s) pending delivery"),
        "/status should route through the same queue handling: {stdout}"
    );
}

/// A real background task started this process shows up in `/queue`'s
/// active/terminal counts once it settles.
#[test]
fn queue_reflects_a_real_background_task() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = offline_chat()
        .env("HOME", &home_path)
        .write_stdin(
            "/background summarize the open questions\nhello\nhello again\n/queue\n/quit\n",
        )
        .output()
        .expect("the chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("0 active, 0 terminal"),
        "a started task should move the queue summary off its zeroed state: {stdout}"
    );
}

/// `--echo` mode has no journal/receipts, so `/steer` and `/interrupt` are
/// refused with a clear message rather than silently no-op'ing. `/queue`
/// still works — it is purely in-memory registry state, not a runtime call.
#[test]
fn steer_and_interrupt_are_unavailable_in_echo_mode() {
    let mut cmd = Command::cargo_bin("ardur").expect("the `ardur` binary builds");
    let output = cmd
        .args(["chat", "--echo"])
        .write_stdin(
            "/steer 00000000-0000-0000-0000-000000000000 hello\n\
             /interrupt 00000000-0000-0000-0000-000000000000\n\
             /quit\n",
        )
        .output()
        .expect("the echo chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let unavailable_count = stdout.matches("unavailable in --echo mode").count();
    assert_eq!(
        unavailable_count, 2,
        "echo mode should refuse both steer and interrupt cleanly: {stdout}"
    );
}
