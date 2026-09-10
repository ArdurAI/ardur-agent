use std::fs;

use assert_cmd::Command;

const SESSION_ID: &str = "018ff6f0-7d5a-7b87-b3d6-6ab7ad0f0001";
const RECEIPT_ID: &str = "11111111-1111-4111-8111-111111111111";
const LATER_RECEIPT_ID: &str = "00000000-0000-4000-8000-000000000001";

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
    write_journal(home.path());

    let list = Command::cargo_bin("ardur")
        .expect("binary")
        .arg("session")
        .arg("list")
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
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
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ARDUR_PROVIDER")
        .env_remove("ARDUR_MODEL")
        .env_remove("ARDUR_DATA_DIR")
        .env("ARDUR_DEV_PERMISSIVE_POLICY", "true")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_COMPAT_API_KEY")
        .env_remove("OLLAMA_BASE_URL")
        .output()
        .expect("resume session");
    assert!(resume.status.success(), "status: {:?}", resume.status);
    let stdout = String::from_utf8_lossy(&resume.stdout);
    assert!(stdout.contains("hello old turn"), "resume output: {stdout}");
    assert!(stdout.contains("old answer"), "resume output: {stdout}");
    assert!(
        stdout.contains("continuing session"),
        "resume output: {stdout}"
    );

    let export = Command::cargo_bin("ardur")
        .expect("binary")
        .arg("session")
        .arg("export")
        .arg(SESSION_ID)
        .arg("--format")
        .arg("markdown")
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .output()
        .expect("export session");
    assert!(export.status.success(), "status: {:?}", export.status);
    let stdout = String::from_utf8_lossy(&export.stdout);
    assert!(
        stdout.contains("# Session Export"),
        "export output: {stdout}"
    );
    assert!(
        stdout.contains(&format!("journals/sessions/{SESSION_ID}/journal.jsonl")),
        "export output: {stdout}"
    );
    assert!(stdout.contains("Receipt"), "export output: {stdout}");
}

#[test]
fn session_list_reports_the_latest_receipt_in_journal_order() {
    let home = tempfile::tempdir().expect("temp HOME");
    let journal = write_journal(home.path());
    fs::write(
        journal,
        format!(
            concat!(
                r#"{{"kind":"AssistantMessage","content":"first","at":1,"receipt_id":"{}"}}"#,
                "\n",
                r#"{{"kind":"AssistantMessage","content":"second","at":2,"receipt_id":"{}"}}"#,
                "\n"
            ),
            RECEIPT_ID, LATER_RECEIPT_ID
        ),
    )
    .expect("ordered receipt journal");

    let output = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["sessions", "list"])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .output()
        .expect("list sessions");
    assert!(output.status.success(), "status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("\"last_receipt_id\": \"{LATER_RECEIPT_ID}\"")),
        "{stdout}"
    );
}

#[test]
fn sessions_resume_replays_and_appends_to_existing_journal() {
    let home = tempfile::tempdir().expect("temp HOME");
    let journal = write_journal(home.path());
    let before = fs::read_to_string(&journal)
        .expect("journal before")
        .lines()
        .count();

    let output = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["sessions", "resume", SESSION_ID])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
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

    let metadata = fs::read_to_string(
        journal
            .parent()
            .expect("session directory")
            .join("metadata.json"),
    )
    .expect("chat writes session metadata");
    let metadata: serde_json::Value = serde_json::from_str(&metadata).expect("metadata json");
    assert_eq!(metadata["session_id"], SESSION_ID);
    assert_eq!(metadata["source"], "cli");
    assert_eq!(metadata["provider"], "anthropic");
    assert!(
        metadata["model"]
            .as_str()
            .is_some_and(|model| !model.is_empty())
    );
}

#[test]
fn sessions_alias_lists_metadata_cost_and_corrupt_receipt_status() {
    let home = tempfile::tempdir().expect("temp HOME");
    let journal = write_journal(home.path());
    fs::write(
        journal.parent().expect("session directory").join("metadata.json"),
        format!(
            r#"{{"session_id":"{SESSION_ID}","created_at_ms":1,"updated_at_ms":2,"provider":"test-provider","model":"test-model","source":"cli","workspace":"test-workspace"}}"#,
        ),
    )
    .expect("metadata");
    let receipt_dir = home.path().join(".ardur/receipts");
    fs::create_dir_all(&receipt_dir).expect("receipt dir");
    fs::write(receipt_dir.join("chain.jsonl"), "broken.not-a-receipt\n").expect("corrupt chain");

    let output = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["sessions", "list"])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .output()
        .expect("list sessions");
    assert!(output.status.success(), "status: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"provider\": \"test-provider\""),
        "{stdout}"
    );
    assert!(stdout.contains("\"model\": \"test-model\""), "{stdout}");
    assert!(stdout.contains("\"source\": \"cli\""), "{stdout}");
    assert!(
        stdout.contains("\"workspace\": \"test-workspace\""),
        "{stdout}"
    );
    assert!(
        stdout.contains("\"cost_cents\": null"),
        "corrupt receipt evidence must surface unknown cost instead of zero: {stdout}"
    );
    assert!(
        stdout.contains("\"receipt_status\": \"corrupt\""),
        "{stdout}"
    );
    assert!(stdout.contains("\"created_at_ms\": 1"), "{stdout}");
    assert!(stdout.contains("\"updated_at_ms\": 2"), "{stdout}");

    let matching = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["sessions", "list", "--workspace", "TEST-WORKSPACE"])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .output()
        .expect("workspace-filtered list");
    assert!(matching.status.success(), "status: {:?}", matching.status);
    assert!(
        String::from_utf8_lossy(&matching.stdout).contains(SESSION_ID),
        "matching workspace must include the session"
    );

    let non_matching = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["sessions", "list", "--workspace", "other-workspace"])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .output()
        .expect("non-matching workspace list");
    assert!(
        non_matching.status.success(),
        "status: {:?}",
        non_matching.status
    );
    assert_eq!(String::from_utf8_lossy(&non_matching.stdout).trim(), "[]");
}

#[test]
fn session_export_redacts_markdown_and_jsonl() {
    let home = tempfile::tempdir().expect("temp HOME");
    let journal = write_journal(home.path());
    fs::write(
        &journal,
        format!(
            concat!(
                r#"{{"kind":"UserMessage","content":"token=abcdefgh12345678 password is hunter2","at":1}}"#,
                "\n",
                r#"{{"kind":"AssistantMessage","content":"secret: super-private-value","at":2,"receipt_id":"{}"}}"#,
                "\n"
            ),
            RECEIPT_ID
        ),
    )
    .expect("sensitive journal");

    for format in ["markdown", "json", "jsonl"] {
        let output = Command::cargo_bin("ardur")
            .expect("binary")
            .args(["sessions", "export", SESSION_ID, "--format", format])
            .env(
                "HOME",
                home.path().canonicalize().expect("canonical temp HOME"),
            )
            .output()
            .expect("export session");
        assert!(
            output.status.success(),
            "format={format}: {:?}",
            output.status
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("<REDACTED>"), "format={format}: {stdout}");
        assert!(
            !stdout.contains("abcdefgh12345678"),
            "format={format}: {stdout}"
        );
        assert!(!stdout.contains("hunter2"), "format={format}: {stdout}");
        assert!(
            !stdout.contains("super-private-value"),
            "format={format}: {stdout}"
        );
        assert!(
            !stdout.contains(&home.path().display().to_string()),
            "format={format}: {stdout}"
        );
        if format == "jsonl" {
            for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
                serde_json::from_str::<serde_json::Value>(line).expect("valid JSONL line");
            }
        } else if format == "json" {
            serde_json::from_str::<serde_json::Value>(&stdout).expect("valid JSON export");
        }
    }

    let output = Command::cargo_bin("ardur")
        .expect("binary")
        .args([
            "sessions",
            "export",
            SESSION_ID,
            "--format",
            "markdown",
            "--output",
            "bundle.md",
        ])
        .current_dir(home.path())
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .output()
        .expect("file export");
    assert!(output.status.success(), "status: {:?}", output.status);
    let bundle_path = home.path().join("bundle.md");
    let bundle = fs::read_to_string(&bundle_path).expect("exported bundle");
    assert!(bundle.contains("<REDACTED>"), "{bundle}");
    assert!(!bundle.contains("hunter2"), "{bundle}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(bundle_path)
            .expect("bundle metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "session exports must be private");
    }
}

#[cfg(unix)]
#[test]
fn session_export_refuses_to_follow_output_symlinks() {
    use std::os::unix::fs::symlink;

    let home = tempfile::tempdir().expect("temp HOME");
    write_journal(home.path());
    let target = home.path().join("sensitive.txt");
    let output_path = home.path().join("bundle.md");
    fs::write(&target, "do not overwrite").expect("target file");
    symlink(&target, &output_path).expect("output symlink");

    let output = Command::cargo_bin("ardur")
        .expect("binary")
        .args([
            "sessions",
            "export",
            SESSION_ID,
            "--format",
            "markdown",
            "--output",
            output_path.to_str().expect("UTF-8 path"),
        ])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .output()
        .expect("export command");

    assert!(!output.status.success(), "symlink export must fail closed");
    assert_eq!(
        fs::read_to_string(&target).expect("target remains readable"),
        "do not overwrite"
    );
}

#[test]
fn session_prune_uses_persisted_activity_instead_of_directory_mtime() {
    let home = tempfile::tempdir().expect("temp HOME");
    let journal = write_journal(home.path());
    let session_dir = journal.parent().expect("session directory");
    fs::write(
        session_dir.join("metadata.json"),
        format!(
            r#"{{"session_id":"{SESSION_ID}","created_at_ms":1,"updated_at_ms":2,"provider":"test","model":"test","source":"cli"}}"#
        ),
    )
    .expect("old persisted metadata");

    let output = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["sessions", "prune", "--older-than", "30", "--confirm"])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .output()
        .expect("confirmed prune");

    assert!(output.status.success(), "status: {:?}", output.status);
    assert!(
        !session_dir.exists(),
        "persisted session age, not recently-created directory mtime, must drive pruning"
    );
}

#[cfg(unix)]
#[test]
fn session_prune_preserves_recent_persisted_activity_when_file_mtimes_are_stale() {
    let home = tempfile::tempdir().expect("temp HOME");
    let journal = write_journal(home.path());
    let session_dir = journal.parent().expect("session directory");
    let metadata_path = session_dir.join("metadata.json");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time after epoch")
        .as_millis();
    fs::write(
        &metadata_path,
        format!(
            r#"{{"session_id":"{SESSION_ID}","created_at_ms":{now_ms},"updated_at_ms":{now_ms},"provider":"test","model":"test","source":"cli"}}"#
        ),
    )
    .expect("recent persisted metadata");

    let stale = std::fs::FileTimes::new()
        .set_accessed(std::time::UNIX_EPOCH)
        .set_modified(std::time::UNIX_EPOCH);
    for path in [&journal, &metadata_path, session_dir] {
        std::fs::File::open(path)
            .expect("open path for timestamp update")
            .set_times(stale)
            .expect("set stale filesystem timestamp");
    }

    let output = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["sessions", "prune", "--older-than", "30", "--confirm"])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .output()
        .expect("confirmed prune");

    assert!(output.status.success(), "status: {:?}", output.status);
    assert!(
        session_dir.exists(),
        "recent persisted activity must protect a session even when copied/restored mtimes are stale"
    );
}

#[test]
fn session_prune_is_dry_run_until_confirmed() {
    let home = tempfile::tempdir().expect("temp HOME");
    let journal = write_journal(home.path());
    let session_dir = journal.parent().expect("session directory").to_path_buf();

    let dry_run = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["sessions", "prune", "--older-than", "0"])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .output()
        .expect("dry-run prune");
    assert!(dry_run.status.success(), "status: {:?}", dry_run.status);
    assert!(session_dir.exists(), "dry-run must not delete the session");
    let stdout = String::from_utf8_lossy(&dry_run.stdout);
    assert!(stdout.contains("dry run"), "{stdout}");
    assert!(stdout.contains("--confirm"), "{stdout}");

    let confirmed = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["sessions", "prune", "--older-than", "0", "--confirm"])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .output()
        .expect("confirmed prune");
    assert!(confirmed.status.success(), "status: {:?}", confirmed.status);
    assert!(
        !session_dir.exists(),
        "confirmed prune must delete the session"
    );
}

#[test]
fn session_list_links_new_journal_receipts_to_the_verified_chain() {
    let home = tempfile::tempdir().expect("temp HOME");
    let session_dir = home
        .path()
        .join(".ardur/journals/sessions")
        .join(SESSION_ID);
    fs::create_dir_all(&session_dir).expect("session directory");
    fs::write(session_dir.join("journal.jsonl"), "").expect("empty journal");

    let chat = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["chat", "--session-id", SESSION_ID, "--no-stream"])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ARDUR_PROVIDER")
        .env_remove("ARDUR_MODEL")
        .env_remove("ARDUR_DATA_DIR")
        .env("ARDUR_DEV_PERMISSIVE_POLICY", "true")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_COMPAT_API_KEY")
        .env_remove("OLLAMA_BASE_URL")
        .write_stdin("receipt-linked turn\n/quit\n")
        .output()
        .expect("chat session");
    assert!(chat.status.success(), "status: {:?}", chat.status);

    let list = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["sessions", "list"])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .output()
        .expect("list sessions");
    assert!(list.status.success(), "status: {:?}", list.status);
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        stdout.contains("\"receipt_status\": \"chain-linked\""),
        "{stdout}"
    );
    assert!(stdout.contains("\"provider\": \"anthropic\""), "{stdout}");
    assert!(stdout.contains("\"model\":"), "{stdout}");
    assert!(stdout.contains("\"cost_cents\": 0"), "{stdout}");

    let json_export = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["sessions", "export", SESSION_ID, "--format", "json"])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .output()
        .expect("JSON export");
    assert!(
        json_export.status.success(),
        "status: {:?}",
        json_export.status
    );
    let bundle: serde_json::Value =
        serde_json::from_slice(&json_export.stdout).expect("valid JSON bundle");
    assert_eq!(bundle["receipt_status"], "chain-linked");
    let evidence = bundle["receipt_evidence"]
        .as_array()
        .expect("receipt evidence array");
    assert_eq!(evidence.len(), 1, "{bundle}");
    let jws = evidence[0]["jws_compact"]
        .as_str()
        .expect("canonical compact JWS");
    assert_eq!(jws.split('.').count(), 3, "{jws}");
    assert_eq!(
        evidence[0]["receipt_id"], evidence[0]["body"]["receipt_id"],
        "decoded body must correspond to the exported signed receipt"
    );

    let markdown_export = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["sessions", "export", SESSION_ID, "--format", "markdown"])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .output()
        .expect("Markdown export");
    assert!(
        markdown_export.status.success(),
        "status: {:?}",
        markdown_export.status
    );
    let markdown = String::from_utf8_lossy(&markdown_export.stdout);
    assert!(markdown.contains("Signed receipt evidence"), "{markdown}");
    assert!(markdown.contains(jws), "{markdown}");
}

#[cfg(unix)]
#[test]
fn session_commands_reject_symlinked_journals() {
    use std::os::unix::fs::symlink;

    let home = tempfile::tempdir().expect("temp HOME");
    let journal = write_journal(home.path());
    let target = home.path().join("outside-journal.jsonl");
    fs::rename(&journal, &target).expect("move journal target outside session directory");
    symlink(&target, &journal).expect("journal symlink");

    for args in [
        vec!["sessions", "list"],
        vec!["sessions", "export", SESSION_ID, "--format", "json"],
        vec!["sessions", "resume", SESSION_ID],
    ] {
        let output = Command::cargo_bin("ardur")
            .expect("binary")
            .args(&args)
            .env(
                "HOME",
                home.path().canonicalize().expect("canonical temp HOME"),
            )
            .output()
            .expect("session command");
        assert!(
            !output.status.success(),
            "{} must reject a symlinked journal",
            args.join(" ")
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("symlink"), "{}: {stderr}", args.join(" "));
    }
}

#[test]
fn session_receipt_status_rejects_hash_linked_but_signature_forged_evidence() {
    let home = tempfile::tempdir().expect("temp HOME");
    let session_dir = home
        .path()
        .join(".ardur/journals/sessions")
        .join(SESSION_ID);
    fs::create_dir_all(&session_dir).expect("session directory");
    fs::write(session_dir.join("journal.jsonl"), "").expect("empty journal");

    let chat = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["chat", "--session-id", SESSION_ID, "--no-stream"])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("ARDUR_PROVIDER")
        .env_remove("ARDUR_MODEL")
        .env_remove("ARDUR_DATA_DIR")
        .env("ARDUR_DEV_PERMISSIVE_POLICY", "true")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("OPENAI_COMPAT_API_KEY")
        .env_remove("OLLAMA_BASE_URL")
        .write_stdin("signed turn\n/quit\n")
        .output()
        .expect("chat session");
    assert!(chat.status.success(), "status: {:?}", chat.status);

    let chain_path = home.path().join(".ardur/receipts/chain.jsonl");
    let mut compact = fs::read_to_string(&chain_path).expect("receipt chain");
    let signature_start = compact.rfind('.').expect("compact JWS signature segment") + 1;
    let first_signature_char = compact.as_bytes()[signature_start] as char;
    compact.replace_range(
        signature_start..=signature_start,
        if first_signature_char == 'A' {
            "B"
        } else {
            "A"
        },
    );
    fs::write(&chain_path, compact).expect("forge signature without changing payload linkage");

    let list = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["sessions", "list"])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .output()
        .expect("list sessions");
    assert!(list.status.success(), "status: {:?}", list.status);
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        stdout.contains("\"receipt_status\": \"corrupt\""),
        "signature-forged evidence must fail closed: {stdout}"
    );

    let export = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["sessions", "export", SESSION_ID, "--format", "json"])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .output()
        .expect("export session");
    assert!(export.status.success(), "status: {:?}", export.status);
    let bundle: serde_json::Value =
        serde_json::from_slice(&export.stdout).expect("valid JSON export");
    assert_eq!(bundle["receipt_status"], "corrupt");
    assert_eq!(bundle["receipt_evidence"], serde_json::json!([]));
}

#[test]
fn session_ids_fail_closed_for_malformed_or_missing_values() {
    let home = tempfile::tempdir().expect("temp HOME");

    let malformed = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["sessions", "resume", "../../escape"])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .output()
        .expect("malformed session id");
    assert!(!malformed.status.success());
    let stderr = String::from_utf8_lossy(&malformed.stderr);
    assert!(stderr.contains("valid UUID"), "{stderr}");

    let missing = Command::cargo_bin("ardur")
        .expect("binary")
        .args(["sessions", "resume", "22222222-2222-4222-8222-222222222222"])
        .env(
            "HOME",
            home.path().canonicalize().expect("canonical temp HOME"),
        )
        .output()
        .expect("missing session");
    assert!(!missing.status.success());
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(stderr.contains("not found"), "{stderr}");
}
