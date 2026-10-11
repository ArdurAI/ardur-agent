#![cfg(unix)]
#[path = "../../home-client/tests/support/mod.rs"]
mod support;
use ardur_eval::home::{HomeScenario, run_scenario};
use ardur_eval::runner::Outcome;
use ardur_eval::transcript::Transcript;
use home_client::{HomeClient, pair_device};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Stdio;
use support::{FakeHome, Mode};
use tokio::io::AsyncWriteExt;

async fn paired(server: &FakeHome) -> HomeClient {
    HomeClient::new(pair_device(&server.code(), "Fixture").await.unwrap()).unwrap()
}
fn case(follow_up: bool) -> HomeScenario {
    HomeScenario::from_yaml(&format!("id: turns\nprompt: first\ntarget: {{kind: bot, bot: bot}}\nexpected: {{exact: 'reply run-{}'}}\n{}", if follow_up { 2 } else { 1 }, if follow_up { "follow_ups: [second]\nmax_turns: 2\n" } else { "" })).unwrap()
}
fn records(dir: &Path) -> Vec<Value> {
    std::fs::read_dir(dir)
        .unwrap()
        .flat_map(|entry| Transcript::recover(&entry.unwrap().path()).unwrap())
        .collect()
}
#[tokio::test]
async fn multi_turn_uses_each_admitted_run_and_never_a_newer_unrelated_answer() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    server.set(Mode::ScenarioTurns);
    let dir = tempfile::tempdir().unwrap();
    let mut transcript = Transcript::create(dir.path()).unwrap();
    let result = run_scenario(&client, &case(true), &mut transcript)
        .await
        .unwrap();
    assert_eq!(result.outcome, Outcome::Pass);
    assert_eq!(result.reply, "reply run-2");
    let saved = records(dir.path());
    let admissions: Vec<_> = saved.iter().filter(|v| v["kind"] == "admission").collect();
    assert_eq!(admissions.len(), 2);
    assert_eq!(admissions[0]["result"]["taskId"], "task-1");
    assert_eq!(admissions[1]["result"]["runId"], "run-2");
    let seen = server.seen.lock().unwrap();
    let requests: Vec<_> = seen
        .iter()
        .filter(|v| v["operation"] == "dispatch")
        .collect();
    assert_eq!(requests.len(), 2); // No grading dispatches or model turns.
    assert_ne!(
        requests[0]["body"]["clientNonce"],
        requests[1]["body"]["clientNonce"]
    );
    assert_eq!(requests[1]["body"]["botId"], "bot");
    let reads: Vec<_> = seen
        .iter()
        .filter(|v| v["operation"] == "messages/get")
        .collect();
    assert_eq!(reads[0]["body"]["around"]["messageId"], "answer-run-1");
    assert_eq!(reads[1]["body"]["around"]["messageId"], "answer-run-2");
}
#[tokio::test]
async fn interrupted_wait_retains_the_admission_for_read_only_recovery() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    server.run.lock().unwrap()["status"] = json!("running");
    let dir = tempfile::tempdir().unwrap();
    let mut transcript = Transcript::create(dir.path()).unwrap();
    let scenario = case(false);
    {
        let future = run_scenario(&client, &scenario, &mut transcript);
        tokio::pin!(future);
        tokio::select! {
            _ = &mut future => panic!("running task must keep waiting"),
            _ = async {
                loop {
                    if records(dir.path()).iter().any(|v| v["kind"] == "admission") { break; }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            } => {}
        }
    }
    // Dropping the future simulates interruption without stopping home work.
    let saved = records(dir.path());
    assert!(
        saved
            .iter()
            .any(|v| v["kind"] == "request" && v["requestId"].as_str().unwrap().len() >= 16)
    );
    assert!(
        saved
            .iter()
            .any(|v| v["kind"] == "admission" && v["result"]["runId"] == "run")
    );
    assert!(!saved.iter().any(|v| v["kind"] == "result"));
    server.run.lock().unwrap()["status"] = json!("completed");
    let run = saved.iter().find(|v| v["kind"] == "admission").unwrap()["result"]["runId"]
        .as_str()
        .unwrap();
    let (answer, exit) = home_client::execute_device(
        &client,
        home_client::DeviceCommand::Wait { run_id: run.into() },
        std::time::Duration::from_secs(5),
    )
    .await;
    assert_eq!(exit, 0);
    assert_eq!(answer.reply_text, "Fixture answer ✓");
    assert_eq!(server.admissions.lock().unwrap().len(), 1);
    assert!(
        !server
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["operation"] == "stop")
    );
}

#[tokio::test]
async fn lost_admission_retains_a_nonce_for_identical_replay_without_duplicate_work() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    server.set(Mode::LostAdmission);
    let dir = tempfile::tempdir().unwrap();
    let mut transcript = Transcript::create(dir.path()).unwrap();
    assert!(matches!(
        run_scenario(&client, &case(false), &mut transcript)
            .await
            .unwrap()
            .outcome,
        Outcome::Unavailable { .. }
    ));
    let saved = records(dir.path());
    let request = saved
        .iter()
        .find(|entry| entry["kind"] == "request")
        .unwrap();
    let (answer, exit) = home_client::execute_device(
        &client,
        home_client::DeviceCommand::Send {
            bot: "bot".into(),
            text: "first".into(),
            request_id: request["requestId"].as_str().unwrap().into(),
            wait: true,
        },
        std::time::Duration::from_secs(5),
    )
    .await;
    assert_eq!(exit, 0);
    assert_eq!(answer.run_id.as_deref(), Some("run"));
    assert_eq!(server.admissions.lock().unwrap().len(), 1);
    let seen = server.seen.lock().unwrap();
    let sends: Vec<_> = seen
        .iter()
        .filter(|entry| entry["operation"] == "dispatch")
        .collect();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0]["body"], sends[1]["body"]);
    assert_ne!(sends[0]["proof"]["nonce"], sends[1]["proof"]["nonce"]);
}
#[tokio::test]
async fn room_runs_are_all_waited_in_order_with_the_resolved_thread() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    server.set(Mode::ScenarioTurns);
    let case = HomeScenario::from_yaml("id: room\nprompt: compare\ntarget: {kind: room, room_id: fixture-room}\nexpected:\n  exact: \"reply fixture-run-1\\nreply fixture-run-2\"\n").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut transcript = Transcript::create(dir.path()).unwrap();
    let result = run_scenario(&client, &case, &mut transcript).await.unwrap();
    assert_eq!(result.outcome, Outcome::Pass);
    let seen = server.seen.lock().unwrap();
    let sends: Vec<_> = seen
        .iter()
        .filter(|v| v["operation"] == "rooms/send")
        .collect();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0]["body"]["threadId"], "fixture-thread");
    let waits: Vec<_> = seen
        .iter()
        .filter(|v| v["operation"] == "runs/get")
        .collect();
    assert_eq!(waits.len(), 2);
}
#[tokio::test]
async fn task_mismatch_is_unavailable_even_if_the_text_would_pass() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    server.run.lock().unwrap()["taskId"] = json!("unrelated-task");
    let case = HomeScenario::from_yaml("id: mismatch\nprompt: first\ntarget: {kind: bot, bot: bot}\nexpected: {exact: 'Fixture answer ✓'}\n").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut transcript = Transcript::create(dir.path()).unwrap();
    assert!(matches!(
        run_scenario(&client, &case, &mut transcript)
            .await
            .unwrap()
            .outcome,
        Outcome::Unavailable { .. }
    ));
}

#[tokio::test]
async fn multi_turn_room_reuses_resolved_thread_on_follow_up_turns() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    server.set(Mode::ScenarioTurns);
    let case = HomeScenario::from_yaml(
        "id: room-turns\nprompt: first\nfollow_ups: [second]\nmax_turns: 2\ntarget: {kind: room, room_id: fixture-room}\nexpected:\n  exact: \"reply fixture-run-1\\nreply fixture-run-2\"\n",
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut transcript = Transcript::create(dir.path()).unwrap();
    let result = run_scenario(&client, &case, &mut transcript).await.unwrap();
    assert_eq!(result.outcome, Outcome::Pass);
    assert_eq!(result.reply, "reply fixture-run-1\nreply fixture-run-2");

    let seen = server.seen.lock().unwrap();
    let sends: Vec<_> = seen
        .iter()
        .filter(|v| v["operation"] == "rooms/send")
        .collect();
    assert_eq!(sends.len(), 2);
    // Thread resolved on turn 1 is reused on turn 2.
    assert_eq!(sends[0]["body"]["threadId"], "fixture-thread");
    assert_eq!(sends[1]["body"]["threadId"], "fixture-thread");
    assert_ne!(
        sends[0]["body"]["clientNonce"],
        sends[1]["body"]["clientNonce"]
    );

    let saved = records(dir.path());
    let requests: Vec<_> = saved.iter().filter(|v| v["kind"] == "request").collect();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["target"]["threadId"], "fixture-thread");
    assert_eq!(requests[1]["target"]["threadId"], "fixture-thread");
}

#[tokio::test]
async fn multi_turn_bot_threads_correlation_fields_and_rejects_mismatch() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    server.set(Mode::ScenarioTurns);
    let bot_name = serde_json::to_string("Bot\u{1b}]0;bad\u{7}").unwrap();
    // Scenario target uses the bot name; admission returns bot id and thread id.
    let case = HomeScenario::from_yaml(&format!(
        "id: bot-turns\nprompt: first\nfollow_ups: [second]\nmax_turns: 2\ntarget: {{kind: bot, bot: {}}}\nexpected: {{exact: 'reply run-2'}}\n",
        bot_name.trim()
    ))
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut transcript = Transcript::create(dir.path()).unwrap();
    let result = run_scenario(&client, &case, &mut transcript).await.unwrap();
    assert_eq!(result.outcome, Outcome::Pass);
    assert_eq!(result.reply, "reply run-2");

    let saved = records(dir.path());
    let admissions: Vec<_> = saved.iter().filter(|v| v["kind"] == "admission").collect();
    assert_eq!(admissions.len(), 2);
    assert_eq!(admissions[0]["result"]["bot"]["id"], "bot");
    assert_eq!(admissions[0]["result"]["bot"]["name"], "Bot");
    assert_eq!(admissions[0]["result"]["data"]["threadId"], "thread");
    assert_eq!(admissions[1]["result"]["bot"]["id"], "bot");
    assert_eq!(admissions[1]["result"]["data"]["threadId"], "thread");

    {
        let seen = server.seen.lock().unwrap();
        let dispatches: Vec<_> = seen
            .iter()
            .filter(|v| v["operation"] == "dispatch")
            .collect();
        assert_eq!(dispatches.len(), 2);
        assert_eq!(dispatches[0]["body"]["botId"], "bot");
        // Turn 2 is sent with the bot id returned by the first admission.
        assert_eq!(dispatches[1]["body"]["botId"], "bot");
        assert_ne!(
            dispatches[0]["body"]["clientNonce"],
            dispatches[1]["body"]["clientNonce"]
        );
    }

    // Mismatched thread, task, or bot id on answer must be rejected as unavailable (home.rs ~341-355).
    for (field, wrong) in [
        ("threadId", "unrelated-thread"),
        ("botId", "unrelated-bot"),
        ("taskId", "unrelated-task"),
    ] {
        let server = FakeHome::start(false).await;
        let client = paired(&server).await;
        server.run.lock().unwrap()[field] = json!(wrong);
        if field == "threadId" {
            server.messages.lock().unwrap()["threadId"] = json!(wrong);
        }
        let mismatch_case = HomeScenario::from_yaml(&format!(
            "id: mismatch\nprompt: first\nfollow_ups: [second]\nmax_turns: 2\ntarget: {{kind: bot, bot: {}}}\nexpected: {{exact: 'Fixture answer ✓'}}\n",
            bot_name.trim()
        ))
        .unwrap();
        let mismatch_dir = tempfile::tempdir().unwrap();
        let mut mismatch_transcript = Transcript::create(mismatch_dir.path()).unwrap();
        let mismatch_result = run_scenario(&client, &mismatch_case, &mut mismatch_transcript)
            .await
            .unwrap();
        assert_eq!(
            mismatch_result.outcome,
            Outcome::Unavailable {
                reasons: vec!["Task or conversation correlation changed.".into()]
            }
        );
    }
}

async fn command(home: &Path, args: &[&str], input: Option<&str>) -> std::process::Output {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_ardur-rs"))
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .await
            .unwrap();
    } else {
        child.stdin.take();
    }
    child.wait_with_output().await.unwrap()
}
#[tokio::test]
async fn command_suite_writes_honest_totals_and_redacts_transcripts_and_reports() {
    let server = FakeHome::start(false).await;
    let dir = tempfile::tempdir().unwrap();
    // The private store refuses symlinked folders; macOS temp paths go through /var.
    let home = dir.path().canonicalize().unwrap();
    assert!(
        command(
            home.as_path(),
            &["pair", "--file", "-"],
            Some(&server.code())
        )
        .await
        .status
        .success()
    );
    let suite = home.as_path().join("suite");
    std::fs::create_dir(&suite).unwrap();
    for (id, expected) in [
        ("pass", "exact: 'Fixture answer ✓'"),
        ("fail", "exact: 'password=fixture-failure'"),
        ("missing", "tool_called: search, cost_under: 0.01"),
    ] {
        std::fs::write(suite.join(format!("{id}.yaml")), format!("id: {id}\ndescription: 'password=fixture-description'\nprompt: 'password=fixture-prompt'\ntarget: {{kind: bot, bot: bot}}\nexpected: {{{expected}}}\n")).unwrap();
    }
    let report = home.as_path().join("report.json");
    let junit = home.as_path().join("report.xml");
    let transcripts = home.as_path().join("transcripts");
    let out = command(
        home.as_path(),
        &[
            "test",
            "run",
            suite.to_str().unwrap(),
            "--json",
            report.to_str().unwrap(),
            "--junit",
            junit.to_str().unwrap(),
            "--transcripts",
            transcripts.to_str().unwrap(),
        ],
        None,
    )
    .await;
    assert_eq!(out.status.code(), Some(1));
    let result: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(
        result["summary"],
        json!({"total":3,"passed":1,"failed":1,"unavailable":1,"errored":0})
    );
    let xml = std::fs::read_to_string(&junit).unwrap();
    assert!(xml.contains("skipped=\"1\""));
    for text in [
        result.to_string(),
        xml,
        serde_json::to_string(&records(&transcripts)).unwrap(),
        String::from_utf8(out.stdout).unwrap(),
    ] {
        for secret in ["fixture-failure", "fixture-description", "fixture-prompt"] {
            assert!(!text.contains(secret), "{text}");
        }
    }
    // Single-file runs accept the same command and keep missing evidence non-green.
    let out = command(
        home.as_path(),
        &[
            "test",
            "run",
            suite.join("missing.yaml").to_str().unwrap(),
            "--transcripts",
            transcripts.to_str().unwrap(),
        ],
        None,
    )
    .await;
    assert_eq!(out.status.code(), Some(2));
}
#[tokio::test]
async fn malformed_suite_never_dispatches_or_echoes_its_source() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.yaml");
    std::fs::write(&bad, "password: fixture-private\nexpected: [").unwrap();
    let out = command(dir.path(), &["test", "run", bad.to_str().unwrap()], None).await;
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        "Malformed scenario.\n"
    );
}

#[tokio::test]
async fn command_interrupt_saves_recovery_and_counts_unstarted_cases_as_unavailable() {
    let server = FakeHome::start(false).await;
    let dir = tempfile::tempdir().unwrap();
    // The private store refuses symlinked folders; macOS temp paths go through /var.
    let home = dir.path().canonicalize().unwrap();
    assert!(
        command(
            home.as_path(),
            &["pair", "--file", "-"],
            Some(&server.code())
        )
        .await
        .status
        .success()
    );
    server.run.lock().unwrap()["status"] = json!("running");
    let suite = home.as_path().join("suite");
    let transcripts = home.as_path().join("transcripts");
    let report = home.as_path().join("report.json");
    std::fs::create_dir(&suite).unwrap();
    for id in ["a", "b"] {
        std::fs::write(
            suite.join(format!("{id}.yaml")),
            format!("id: {id}\nprompt: hello\ntarget: {{kind: bot, bot: bot}}\n"),
        )
        .unwrap();
    }
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_ardur-rs"))
        .args([
            "test",
            "run",
            suite.to_str().unwrap(),
            "--json",
            report.to_str().unwrap(),
            "--transcripts",
            transcripts.to_str().unwrap(),
        ])
        .env("HOME", home.as_path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if transcripts.is_dir()
                && records(&transcripts)
                    .iter()
                    .any(|r| r["kind"] == "admission")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("admission must be persisted before waiting");
    // Signal only the process this test started.
    let pid = child.id().unwrap();
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) }, 0);
    let out = child.wait_with_output().await.unwrap();
    assert_eq!(out.status.code(), Some(130));
    let result: Value = serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
    assert_eq!(result["summary"]["total"], 2);
    assert_eq!(result["summary"]["unavailable"], 2);
    assert!(
        records(&transcripts)
            .iter()
            .any(|r| r["kind"] == "admission" && r["result"]["runId"] == "run")
    );
    assert!(
        !server
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["operation"] == "stop")
    );
}

#[tokio::test]
async fn scenario_home_update_refusal_keeps_the_distinct_exit_code() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    server.set(Mode::UnsupportedOperation);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    // A pairing code is redeemed once, so the CLI pairs with its own fake home.
    let cli_server = FakeHome::start(false).await;
    assert!(
        command(&root, &["pair", "--file", "-"], Some(&cli_server.code()))
            .await
            .status
            .success()
    );
    cli_server.set(Mode::UnsupportedOperation);
    let mut transcript = Transcript::create(dir.path()).unwrap();
    let result = run_scenario(&client, &case(false), &mut transcript)
        .await
        .unwrap();
    assert!(result.home_update_required);
    let Outcome::Unavailable { reasons } = result.outcome else {
        panic!("unsupported operation cannot be graded")
    };
    assert_eq!(
        reasons,
        ["Home does not support bots/list; the home must be updated."]
    );
    let scenario = root.join("scenario.yaml");
    std::fs::write(
        &scenario,
        "id: compatibility\nprompt: first\ntarget: {kind: bot, bot: bot}\n",
    )
    .unwrap();
    let transcripts = root.join("cli-transcripts");
    let output = command(
        &root,
        &[
            "test",
            "run",
            scenario.to_str().unwrap(),
            "--transcripts",
            transcripts.to_str().unwrap(),
        ],
        None,
    )
    .await;
    assert_eq!(output.status.code(), Some(5));
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("Home does not support bots/list; the home must be updated.")
    );
}
