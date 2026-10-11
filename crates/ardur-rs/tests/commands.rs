#![cfg(unix)]
#[path = "../../home-client/tests/support/mod.rs"]
mod support;
use serde_json::{Value, json};
use std::process::Stdio;
use support::{FakeHome, Mode};
use tokio::io::AsyncWriteExt;
async fn run(home: &std::path::Path, args: &[&str], input: Option<&str>) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_ardur-rs"));
    command
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::piped());
    let mut child = command.spawn().unwrap();
    if let Some(text) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(text.as_bytes())
            .await
            .unwrap();
    } else {
        child.stdin.take();
    }
    child.wait_with_output().await.unwrap()
}
fn object(out: &std::process::Output) -> Value {
    let text = std::str::from_utf8(&out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(!text.contains("sensitive-canary"));
    assert!(!text.contains("PRIVATE KEY"));
    assert!(out.stderr.is_empty());
    let v: Value = serde_json::from_str(text).unwrap();
    assert_eq!(v["schemaVersion"], 1);
    v
}
#[tokio::test]
async fn installed_binary_pairs_from_stdin_and_file_then_reads_signed_state() {
    for stdin in [true, false] {
        let server = FakeHome::start(false).await;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let file = root.join("pair-code.txt");
        std::fs::write(&file, server.code()).unwrap();
        let out = if stdin {
            run(
                &root,
                &["pair", "--file", "-", "--name", "Test CLI", "--json"],
                Some(&server.code()),
            )
            .await
        } else {
            run(
                &root,
                &["--json", "pair", "--file", file.to_str().unwrap()],
                None,
            )
            .await
        };
        assert!(out.status.success());
        assert_eq!(
            object(&out)["data"],
            json!({"homeName":home_client::safe_output(&server.payload.home_name),"instanceId":"fake-home","paired":true})
        );
        for command in [vec!["status", "--json"], vec!["bots", "list", "--json"]] {
            let out = run(&root, &command, None).await;
            assert!(out.status.success());
            assert_eq!(object(&out)["ok"], true);
        }
        for command in [vec!["status"], vec!["bots", "list"]] {
            let out = run(&root, &command, None).await;
            assert!(out.status.success());
            assert!(!out.stdout.contains(&0x1b));
            assert!(!out.stdout.contains(&0x07));
            assert!(!String::from_utf8_lossy(&out.stdout).contains("sensitive-canary"));
        }
        let seen = server.seen.lock().unwrap();
        assert_eq!(seen[0].as_object().unwrap().len(), 2); // grantId omitted before pairing
        assert_eq!(seen[1]["platform"], "cli");
        assert!(seen[1]["deviceName"] == "Test CLI" || seen[1]["deviceName"] == "Command line");
    }
}
#[tokio::test]
async fn failures_emit_one_safe_envelope_and_correct_exit() {
    let server = FakeHome::start(false).await;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    for args in [
        vec!["status", "--json"],
        vec!["--json", "--sensitive-canary"],
        vec!["pair", "--file", "-", "--json"],
    ] {
        let out = run(&root, &args, Some("sensitive-canary")).await;
        assert!(!out.status.success());
        let v = object(&out);
        assert_eq!(v["ok"], false);
        assert_eq!(v["exitCode"], out.status.code().unwrap());
    }
    let out = run(
        &root,
        &["pair", "--file", "-", "--json"],
        Some(&server.code()),
    )
    .await;
    assert!(out.status.success());
    server.set(Mode::SecretError);
    let out = run(&root, &["status", "--json"], None).await;
    assert_eq!(out.status.code(), Some(1));
    object(&out);
    server.set(Mode::Revoked);
    let out = run(&root, &["bots", "list", "--json"], None).await;
    assert_eq!(out.status.code(), Some(2));
    object(&out);
}

fn stage3_object(out: &std::process::Output) -> Value {
    let text = std::str::from_utf8(&out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(!text.contains("sensitive-canary"));
    assert!(!text.contains("PRIVATE KEY"));
    assert!(out.stderr.is_empty());
    let result: Value = serde_json::from_str(text).unwrap();
    assert_eq!(result["version"], 1);
    for field in [
        "command",
        "bot",
        "runId",
        "taskId",
        "verdict",
        "replyText",
        "elapsedMs",
        "failureReason",
        "data",
    ] {
        assert!(result.get(field).is_some());
    }
    result
}
async fn setup() -> (FakeHome, tempfile::TempDir, std::path::PathBuf) {
    let server = FakeHome::start(false).await;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let out = run(
        &root,
        &["pair", "--file", "-", "--json"],
        Some(&server.code()),
    )
    .await;
    assert!(out.status.success());
    (server, temp, root)
}
#[tokio::test]
async fn binary_stage3_commands_recovery_and_exact_envelope() {
    let (server, _temp, root) = setup().await;
    let sent = run(
        &root,
        &[
            "send",
            "bot",
            "Reply ✓ 🚀",
            "--request-id",
            "stable-request-id-001",
            "--wait",
            "--json",
        ],
        None,
    )
    .await;
    assert!(sent.status.success());
    let result = stage3_object(&sent);
    assert_eq!(result["replyText"], "Fixture answer ✓");
    assert_eq!(result["command"], "send");
    for (args, name) in [
        (vec!["wait", "--run", "run", "--json"], "wait"),
        (vec!["runs", "list", "--json"], "runs list"),
        (vec!["runs", "show", "run", "--json"], "runs show"),
        (vec!["tasks", "show", "task", "--json"], "tasks show"),
        (vec!["stop", "task", "--json"], "stop"),
    ] {
        let out = run(&root, &args, None).await;
        assert!(out.status.success());
        assert_eq!(stage3_object(&out)["command"], name);
    }
    let out = run(&root, &["stop", "task"], None).await;
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "Cancellation requested for task.\n"
    );
    assert_eq!(server.admissions.lock().unwrap().len(), 1);
    let out = run(
        &root,
        &[
            "send",
            "bot",
            "Reply ✓ 🚀",
            "--request-id",
            "stable-request-id-001",
            "--json",
        ],
        None,
    )
    .await;
    assert!(out.status.success());
    assert_eq!(stage3_object(&out)["taskId"], "task");
    let out = run(
        &root,
        &[
            "send",
            "bot",
            "Changed",
            "--request-id",
            "stable-request-id-001",
            "--json",
        ],
        None,
    )
    .await;
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(stage3_object(&out)["verdict"], "error");
}
#[tokio::test]
async fn binary_deadline_missing_answer_revocation_and_safe_usage_exits() {
    let (server, _temp, root) = setup().await;
    for args in [
        vec!["send", "bot", "sensitive-canary", "--json"],
        vec!["wait", "--run", "run", "--timeout", "0", "--json"],
        vec!["wait", "--run", "run", "--timeout", "1.s", "--json"],
        vec!["runs", "list", "--limit", "101", "--json"],
        vec!["--json", "wait", "--sensitive-canary"],
    ] {
        let out = run(&root, &args, None).await;
        assert_eq!(out.status.code(), Some(4));
        stage3_object(&out);
    }
    server.run.lock().unwrap()["status"] = json!("running");
    server.run.lock().unwrap()["state"] = json!("running");
    let out = run(
        &root,
        &["wait", "--run", "run", "--timeout", "100ms", "--json"],
        None,
    )
    .await;
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(stage3_object(&out)["verdict"], "deadline");
    assert!(
        !server
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["operation"] == "stop")
    );
    server.run.lock().unwrap()["status"] = json!("completed");
    server.run.lock().unwrap()["state"] = json!("done");
    server.run.lock().unwrap()["messageId"] = Value::Null;
    let out = run(&root, &["wait", "--run", "run", "--json"], None).await;
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(stage3_object(&out)["verdict"], "error");
    server.set(Mode::Revoked);
    let out = run(&root, &["runs", "show", "run", "--json"], None).await;
    assert_eq!(out.status.code(), Some(4));
    stage3_object(&out);
}

#[tokio::test]
async fn binary_stage4_commands_answers_and_room_send_recovery() {
    let (server, _temp, root) = setup().await;
    for (args, name) in [
        (vec!["computers", "list", "--json"], "computers list"),
        (
            vec!["board", "list", "--workspace", "fixture-board", "--json"],
            "board list",
        ),
        (
            vec![
                "board",
                "show",
                "--workspace",
                "fixture-board",
                "work-1",
                "--json",
            ],
            "board show",
        ),
        (vec!["rooms", "list", "--json"], "rooms list"),
    ] {
        let out = run(&root, &args, None).await;
        assert!(out.status.success());
        assert_eq!(stage3_object(&out)["command"], name);
    }
    let out = run(
        &root,
        &[
            "board",
            "list",
            "--workspace",
            "fixture-board",
            "--filter",
            "{\"status\":\"open\"}",
            "--search",
            "Fixture",
            "--json",
        ],
        None,
    )
    .await;
    assert!(out.status.success());
    assert!(
        server
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["operation"] == "rpc"
                && v["body"]["procedure"] == "board/snapshot"
                && v["body"]["input"]["filter"] == json!({"status":"open"})
                && v["body"]["input"]["search"] == "Fixture")
    );
    // A work answer keeps every run id; human output lists them all.
    let out = run(
        &root,
        &["rooms", "send", "--room", "Fixture room", "hello", "--json"],
        None,
    )
    .await;
    assert!(out.status.success());
    let result = stage3_object(&out);
    assert_eq!(result["command"], "rooms send");
    assert_eq!(
        result["data"]["runIds"],
        json!(["fixture-run-1", "fixture-run-2"])
    );
    let out = run(
        &root,
        &[
            "rooms",
            "send",
            "--room-id",
            "fixture-room",
            "--thread",
            "fixture-thread",
            "hello",
        ],
        None,
    )
    .await;
    assert!(out.status.success());
    // Human output escapes terminal controls, including newlines, like Stage 3.
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "Task fixture-task\\u{a}Runs fixture-run-1, fixture-run-2\n"
    );
    // A lost response is recovered by rerunning the identical command: the home
    // returns the original admission under the same clientNonce.
    server.set(Mode::LostRoomAdmission);
    let out = run(
        &root,
        &[
            "rooms",
            "send",
            "--room",
            "Fixture room",
            "recover me",
            "--json",
        ],
        None,
    )
    .await;
    assert_eq!(out.status.code(), Some(2));
    server.set(Mode::Normal);
    let out = run(
        &root,
        &[
            "rooms",
            "send",
            "--room",
            "Fixture room",
            "recover me",
            "--json",
        ],
        None,
    )
    .await;
    assert!(out.status.success());
    assert_eq!(stage3_object(&out)["taskId"], "fixture-task");
    {
        let seen = server.seen.lock().unwrap();
        let sends: Vec<_> = seen
            .iter()
            .filter(|v| v["operation"] == "rooms/send" && v["body"]["text"] == "recover me")
            .collect();
        assert_eq!(sends.len(), 2);
        assert_eq!(sends[0]["body"], sends[1]["body"]);
        assert_ne!(sends[0]["proof"]["nonce"], sends[1]["proof"]["nonce"]);
    }
    // Ambiguous room names refuse with the home's own answer.
    server.rooms.lock().unwrap().as_array_mut().unwrap().push(
        json!({"id":"fixture-room-2","spaceId":"fixture-space","name":"Fixture room","pinned":false,"sectionId":null,"archivedAt":null,"threadId":"fixture-thread-2","preview":"","unread":false,"members":[],"updatedAt":"2026-10-01T00:00:00.000Z","createdAt":"2026-10-01T00:00:00.000Z"}),
    );
    let out = run(
        &root,
        &["rooms", "send", "--room", "Fixture room", "hello", "--json"],
        None,
    )
    .await;
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(
        stage3_object(&out)["failureReason"],
        "This record is unavailable from this device."
    );
    server.rooms.lock().unwrap().as_array_mut().unwrap().pop();
    // Board denials print the home's fixed sentence.
    server.set(Mode::BoardDenied);
    let out = run(
        &root,
        &[
            "board",
            "show",
            "--workspace",
            "fixture-board",
            "work-1",
            "--json",
        ],
        None,
    )
    .await;
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(
        stage3_object(&out)["failureReason"],
        "This board is only available to this computer's owner."
    );
    // A receipt-only greeting invents no run and prints its text honestly.
    server.set(Mode::Normal);
    *server.room_result.lock().unwrap() = json!({"kind":"receipt-only","seq":2,"receipt":{"id":"fixture-receipt","threadId":"fixture-thread","seq":3,"botId":"fixture-chief","requestMessageId":"fixture-message","key":"greeting","text":"Hello.","createdAt":"2026-10-01T00:00:00.000Z"}});
    let out = run(
        &root,
        &["rooms", "send", "--room", "Fixture room", "hi", "--json"],
        None,
    )
    .await;
    assert!(out.status.success());
    assert_eq!(stage3_object(&out)["data"]["kind"], "receipt-only");
    let out = run(
        &root,
        &["rooms", "send", "--room", "Fixture room", "hi"],
        None,
    )
    .await;
    assert!(out.status.success());
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "Hello.\n");
    // Missing or conflicting room flags exit 4 with the safe envelope.
    for args in [
        vec!["rooms", "send", "hello", "--json"],
        vec![
            "rooms",
            "send",
            "--room",
            "a",
            "--room-id",
            "b",
            "hello",
            "--json",
        ],
    ] {
        let out = run(&root, &args, None).await;
        assert_eq!(out.status.code(), Some(4));
        stage3_object(&out);
    }
}

#[tokio::test]
async fn saved_answer_corpus_never_leaks_credentials_on_stdout() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../home-client/tests/fixtures/redaction.json"
    ))
    .unwrap();
    let saved = &fixture["savedAnswer"];
    let server = FakeHome::start(false).await;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let out = run(
        &root,
        &["pair", "--file", "-", "--json"],
        Some(&server.code()),
    )
    .await;
    assert!(out.status.success());
    server.messages.lock().unwrap()["messages"][0]["blocks"][0]["text"] = saved["input"].clone();
    for json_mode in [false, true] {
        for command in [
            vec![
                "send",
                "bot",
                "Fixture",
                "--request-id",
                "corpus-request-001",
                "--wait",
            ],
            vec!["wait", "--run", "run"],
        ] {
            let mut args = command;
            if json_mode {
                args.push("--json");
            }
            let out = run(&root, &args, None).await;
            assert_eq!(out.status.code(), Some(0));
            assert!(out.stderr.is_empty());
            let stdout = std::str::from_utf8(&out.stdout).unwrap();
            for credential in saved["credentials"].as_array().unwrap() {
                assert!(
                    !stdout.contains(credential.as_str().unwrap()),
                    "synthetic credential leaked"
                );
            }
            if json_mode {
                assert_eq!(stage3_object(&out)["replyText"], saved["expected"]);
            } else {
                let expected = home_client::human_text(saved["expected"].as_str().unwrap()) + "\n";
                assert_eq!(stdout, expected);
            }
        }
    }
}

#[tokio::test]
async fn events_command_prints_jsonl_and_readable_lines_from_signed_windows() {
    for json_mode in [true, false] {
        let server = FakeHome::start(false).await;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        assert!(
            run(
                &root,
                &["pair", "--file", "-", "--json"],
                Some(&server.code())
            )
            .await
            .status
            .success()
        );
        let text = format!(
            "id: 4\nevent: event\ndata: {}\n\nevent: window\ndata: {{\"nextCursor\":7,\"reason\":\"timeout\"}}\n\n",
            json!({"seq":4,"runId":"run","threadId":"thread","botId":"bot","type":"thread.progress","payload":{"text":"Reply ✓ 🚀 token=private-canary\u{1b}[2J"}})
        );
        server
            .event_windows
            .lock()
            .unwrap()
            .push_back(support::EventResponse {
                chunks: vec![text.into_bytes()],
                interrupted: false,
                finish_run: false,
            });
        let mut args = vec!["runs", "events", "run", "--cursor", "1"];
        if json_mode {
            args.push("--json");
        }
        let out = run(&root, &args, None).await;
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(!text.contains("private-canary"));
        assert!(!text.contains('\u{1b}'));
        assert!(text.contains("Reply ✓ 🚀"));
        assert_eq!(text.lines().count(), 2);
        if json_mode {
            let lines: Vec<Value> = text
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(lines[0]["event"], "event");
            assert_eq!(lines[0]["cursor"], 4);
            assert_eq!(lines[1]["data"]["nextCursor"], 7);
        }
        let seen = server.seen.lock().unwrap();
        assert_eq!(
            seen.iter().find(|r| r["operation"] == "events").unwrap()["body"]["cursor"],
            1
        );
    }
}

#[tokio::test]
async fn ctrl_c_stops_the_events_process_cleanly_with_its_last_cursor() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let server = FakeHome::start(false).await;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    assert!(
        run(
            &root,
            &["pair", "--file", "-", "--json"],
            Some(&server.code())
        )
        .await
        .status
        .success()
    );
    server.run.lock().unwrap()["status"] = json!("running");
    server
        .event_windows
        .lock()
        .unwrap()
        .push_back(support::EventResponse {
            chunks: vec![
                b"event: window\ndata: {\"nextCursor\":7,\"reason\":\"timeout\"}\n\n".to_vec(),
            ],
            interrupted: false,
            finish_run: false,
        });
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_ardur-rs"))
        .args(["runs", "events", "run", "--follow", "--json"])
        .env("HOME", &root)
        .env_remove("XDG_CONFIG_HOME")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&first).unwrap()["data"]["nextCursor"],
        7
    );
    // Signal only the subprocess created by this test.
    assert_eq!(
        unsafe { libc::kill(child.id().unwrap() as i32, libc::SIGINT) },
        0
    );
    let last = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let last: Value = serde_json::from_str(&last).unwrap();
    assert_eq!(last["data"]["reason"], "interrupted");
    assert_eq!(last["data"]["nextCursor"], 7);
    let out = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stderr.is_empty());
}
