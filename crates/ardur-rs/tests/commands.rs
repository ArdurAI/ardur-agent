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
            json!({"homeName":server.payload.home_name,"instanceId":"fake-home","paired":true})
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
