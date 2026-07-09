use std::fs;

use assert_cmd::Command;

const SESSION_ID: &str = "018ff6f0-7d5a-7b87-b3d6-6ab7ad0f0001";
const RECEIPT_ID: &str = "11111111-1111-4111-8111-111111111111";

fn write_journal(home: &std::path::Path) -> std::path::PathBuf {
    let dir = home
        .join(".ardur")
        .join("journals")
        .join("sessions")
        .join(SESSION_ID);
    fs::create_dir_all(&dir).expect("session dir");
    let journal = dir.join("journal.jsonl");
    fs::write(
        &journal,
        format!(
            concat!(
                r#"{{"kind":"UserMessage","content":"hello old turn","at":1}}"#,
                "\n",
                r#"{{"kind":"AssistantMessage","content":"old answer","at":2,"receipt_id":"{}"}}"#,
                "\n"
            ),
            RECEIPT_ID
        ),
    )
    .expect("write journal");
    journal
}

#[test]
fn session_commands_list_resume_and_export_real_journal_entries() {
    let home = tempfile::tempdir().expect("temp HOME");
    let journal = write_journal(home.path());

    let list = Command::cargo_bin("ardur")
        .expect("binary")
        .arg("session")
        .arg("list")
        .env("HOME", home.path())
        .output()
        .expect("list sessions");
    assert!(list.status.success(), "status: {:?}", list.status);
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains(SESSION_ID), "list output: {stdout}");
    assert!(stdout.contains("\"turns\": 1"), "list output: {stdout}");
    assert!(stdout.contains(RECEIPT_ID), "list output: {stdout}");

    let resume = Command::cargo_bin("ardur")
        .expect("binary")
        .arg("session")
        .arg("resume")
        .arg(SESSION_ID)
        .env("HOME", home.path())
        .output()
        .expect("resume session");
    assert!(resume.status.success(), "status: {:?}", resume.status);
    let stdout = String::from_utf8_lossy(&resume.stdout);
    assert!(stdout.contains("hello old turn"), "resume output: {stdout}");
    assert!(stdout.contains("old answer"), "resume output: {stdout}");
    assert!(
        stdout.contains("ardur chat --session-id"),
        "resume output: {stdout}"
    );

    let export = Command::cargo_bin("ardur")
        .expect("binary")
        .arg("session")
        .arg("export")
        .arg(SESSION_ID)
        .arg("--format")
        .arg("markdown")
        .env("HOME", home.path())
        .output()
        .expect("export session");
    assert!(export.status.success(), "status: {:?}", export.status);
    let stdout = String::from_utf8_lossy(&export.stdout);
    assert!(
        stdout.contains("# Session Export"),
        "export output: {stdout}"
    );
    assert!(
        stdout.contains(&journal.display().to_string()),
        "export output: {stdout}"
    );
    assert!(stdout.contains("Receipt"), "export output: {stdout}");
}

#[test]
fn chat_session_id_replays_and_appends_to_existing_journal() {
    let home = tempfile::tempdir().expect("temp HOME");
    let journal = write_journal(home.path());
    let before = fs::read_to_string(&journal)
        .expect("journal before")
        .lines()
        .count();

    let output = Command::cargo_bin("ardur")
        .expect("binary")
        .arg("chat")
        .arg("--session-id")
        .arg(SESSION_ID)
        .env("HOME", home.path())
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ARDUR_PROVIDER")
        .env_remove("ARDUR_MODEL")
        .env_remove("ARDUR_DATA_DIR")
        .env("ARDUR_DEV_PERMISSIVE_POLICY", "true")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_COMPAT_API_KEY")
        .env_remove("OLLAMA_BASE_URL")
        .write_stdin("new resumed turn\n/quit\n")
        .output()
        .expect("resumed chat");

    assert!(output.status.success(), "status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("resumed session"), "chat output: {stdout}");
    assert!(stdout.contains("prior messages"), "chat output: {stdout}");
    assert!(stdout.contains("[anthropic stub]"), "chat output: {stdout}");

    let after_contents = fs::read_to_string(&journal).expect("journal after");
    assert!(
        after_contents.lines().count() > before,
        "resumed chat should append to the original journal: {after_contents}"
    );
    assert!(
        after_contents.contains("new resumed turn"),
        "new user turn should be durable in the original journal: {after_contents}"
    );
}
