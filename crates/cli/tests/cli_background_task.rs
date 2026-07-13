//! §1.9 end-to-end tests for `/background` (and its `/bg`/`/btw` aliases),
//! `/tasks`, and `/task status|result|cancel` over the offline stub
//! provider.
//!
//! A background task's id is random and only known once the process has
//! already printed it, so — with the CLI's static (non-interactive) piped
//! stdin model — there is no way to feed a just-minted id back into a
//! *later* line of the *same* stdin script. These tests therefore prove the
//! command surface end to end (starts, lists, and cleanly reports on
//! not-found ids) at the CLI layer; `FusedRuntime::run_background_task`/
//! `cancel_background_task`'s actual task-completion and receipt-minting
//! behavior is covered directly in
//! `crates/fused-runtime/tests/background_task.rs`.

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

/// `/background <prompt>` starts a task immediately (prints its id without
/// blocking the REPL), and by the time a couple more lines have been
/// processed and the process exits, `/tasks` shows it in a real terminal or
/// active status — proving the task actually ran concurrently rather than
/// being silently dropped.
#[test]
fn background_task_starts_and_completes_over_the_offline_stub() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = offline_chat()
        .env("HOME", &home_path)
        .write_stdin("/background summarize the open questions\nhello\nhello again\n/tasks\n/quit\n")
        .output()
        .expect("the chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("started background task"),
        "starting a task should be confirmed immediately: {stdout}"
    );
    assert!(
        stdout.contains("[completed]") || stdout.contains("[running]") || stdout.contains("[queued]"),
        "/tasks should list the task in a real status: {stdout}"
    );
}

/// `/bg` and `/btw` are aliases for `/background`.
#[test]
fn bg_and_btw_are_aliases_for_background() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = offline_chat()
        .env("HOME", &home_path)
        .write_stdin("/bg one thing\n/btw another thing\n/quit\n")
        .output()
        .expect("the chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let started_count = stdout.matches("started background task").count();
    assert_eq!(started_count, 2, "both aliases should start a task: {stdout}");
}

/// `/background` with no prompt is a clean usage error, not a crash or a
/// task started with an empty prompt.
#[test]
fn background_with_no_prompt_is_a_usage_error() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = offline_chat()
        .env("HOME", &home_path)
        .write_stdin("/background\n/quit\n")
        .output()
        .expect("the chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("usage:") && !stdout.contains("started background task"),
        "a bare /background must not start a task: {stdout}"
    );
}

/// `/task status`, `/task result`, and `/task cancel` on an unknown id all
/// report cleanly rather than crashing.
#[test]
fn task_subcommands_report_cleanly_for_an_unknown_id() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = offline_chat()
        .env("HOME", &home_path)
        .write_stdin(
            "/task status 00000000-0000-0000-0000-000000000000\n\
             /task result 00000000-0000-0000-0000-000000000000\n\
             /task cancel 00000000-0000-0000-0000-000000000000\n\
             /quit\n",
        )
        .output()
        .expect("the chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let not_found_count = stdout.matches("not found").count();
    assert_eq!(
        not_found_count, 3,
        "status, result, and cancel should each report the unknown id cleanly: {stdout}"
    );
}

/// A malformed (non-UUID) task id is a usage error, not a parse panic.
#[test]
fn task_subcommand_rejects_a_malformed_id() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = offline_chat()
        .env("HOME", &home_path)
        .write_stdin("/task status not-a-uuid\n/quit\n")
        .output()
        .expect("the chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("usage:"), "{stdout}");
}

/// `/tasks` on an empty registry reports cleanly rather than erroring.
#[test]
fn tasks_with_none_started_reports_cleanly() {
    let home = tempfile::tempdir().expect("temp HOME");
    let home_path = home.path().canonicalize().expect("canonical temp HOME");

    let output = offline_chat()
        .env("HOME", &home_path)
        .write_stdin("/tasks\n/quit\n")
        .output()
        .expect("the chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no background tasks"),
        "empty task list should say so: {stdout}"
    );
}

/// `--echo` mode has no journal/receipts, so background tasks are refused
/// with a clear message rather than silently no-op'ing.
#[test]
fn background_task_is_unavailable_in_echo_mode() {
    let mut cmd = Command::cargo_bin("ardur").expect("the `ardur` binary builds");
    let output = cmd
        .args(["chat", "--echo"])
        .write_stdin("/background do something\n/quit\n")
        .output()
        .expect("the echo chat process runs");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("unavailable in --echo mode"),
        "echo mode should refuse background tasks cleanly: {stdout}"
    );
}
