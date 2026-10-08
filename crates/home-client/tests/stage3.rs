mod support;
use home_client::{DeviceCommand, HomeClient, execute_device, pair_device};
use serde_json::{Value, json};
use std::time::Duration;
use support::{FakeHome, Mode};

async fn paired(server: &FakeHome) -> HomeClient {
    HomeClient::new(pair_device(&server.code(), "Fixture").await.unwrap()).unwrap()
}
fn send(wait: bool, text: &str) -> DeviceCommand {
    DeviceCommand::Send {
        bot: "bot".into(),
        text: text.into(),
        request_id: "stable-request-id-001".into(),
        wait,
    }
}
fn wait() -> DeviceCommand {
    DeviceCommand::Wait {
        run_id: "run".into(),
    }
}
async fn run(client: &HomeClient, command: DeviceCommand) -> (Value, i32) {
    let (result, exit) = execute_device(client, command, Duration::from_secs(5)).await;
    let output = result.json();
    let text = output.to_string();
    assert!(!text.contains("sensitive-canary"));
    assert!(!text.contains("PRIVATE KEY"));
    assert_eq!(output["version"], 1);
    (output, exit)
}
#[tokio::test]
async fn all_six_commands_use_signed_home_operations_and_exact_answers() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    let (sent, exit) = run(&client, send(true, "Reply ✓ 🚀")).await;
    assert_eq!(exit, 0);
    assert_eq!(sent["replyText"], "Fixture answer ✓");
    assert_eq!(sent["runId"], "run");
    assert_eq!(sent["taskId"], "task");
    let (resumed, exit) = run(&client, wait()).await;
    assert_eq!(exit, 0);
    assert_eq!(resumed["replyText"], "Fixture answer ✓");
    for command in [
        DeviceCommand::RunsShow {
            run_id: "run".into(),
        },
        DeviceCommand::TasksShow {
            task_id: "task".into(),
        },
        DeviceCommand::RunsList {
            cursor: Some("cursor".into()),
            limit: 2,
        },
        DeviceCommand::Stop {
            task_id: "task".into(),
        },
    ] {
        assert_eq!(run(&client, command).await.1, 0);
    }
    let seen = server.seen.lock().unwrap();
    assert!(seen.iter().any(|v| v["operation"] == "dispatch"
        && v["body"]
            == json!({"clientNonce":"stable-request-id-001","botId":"bot","text":"Reply ✓ 🚀"})));
    assert!(seen.iter().any(
        |v| v["operation"] == "runs/list" && v["body"] == json!({"cursor":"cursor","limit":2})
    ));
    assert!(seen.iter().any(|v| v["operation"] == "messages/get"
        && v["body"]
            == json!({"botId":"bot","threadId":"thread","around":{"messageId":"answer"}})));
    assert!(
        !seen
            .iter()
            .any(|v| v["operation"] == "tasks" || v["operation"] == "summaries")
    );
}
#[tokio::test]
async fn lost_response_recovers_identical_input_with_fresh_proofs_and_no_blind_retry() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    server.set(Mode::LostAdmission);
    assert_eq!(run(&client, send(false, "Same task")).await.1, 2);
    assert_eq!(server.admissions.lock().unwrap().len(), 1);
    assert_eq!(
        server
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["operation"] == "dispatch")
            .count(),
        1
    );
    let (recovered, exit) = run(&client, send(true, "Same task")).await;
    assert_eq!(exit, 0);
    assert_eq!(recovered["taskId"], "task");
    assert_eq!(run(&client, send(false, "Changed task")).await.1, 4);
    assert_eq!(server.admissions.lock().unwrap().len(), 1);
    let seen = server.seen.lock().unwrap();
    let sends: Vec<_> = seen
        .iter()
        .filter(|v| v["operation"] == "dispatch")
        .collect();
    assert_eq!(sends[0]["body"], sends[1]["body"]);
    assert_ne!(sends[0]["proof"]["nonce"], sends[1]["proof"]["nonce"]);
    assert_ne!(
        sends[0]["proof"]["signature"],
        sends[1]["proof"]["signature"]
    );
}
#[tokio::test]
async fn stop_request_and_requested_wait_do_not_claim_confirmation() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    server.run.lock().unwrap()["status"] = json!("running");
    server.run.lock().unwrap()["state"] = json!("running");
    let (stop, exit) = execute_device(
        &client,
        DeviceCommand::Stop {
            task_id: "task".into(),
        },
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(exit, 0);
    assert_eq!(stop.human(), "Cancellation requested for task.");
    let (shown, exit) = run(
        &client,
        DeviceCommand::RunsShow {
            run_id: "run".into(),
        },
    )
    .await;
    assert_eq!(exit, 0);
    assert_eq!(shown["data"]["run"]["cancelRequested"], true);
    assert_eq!(shown["data"]["run"]["cancelConfirmed"], false);
    server.set(Mode::RunningThenCancelled);
    let (result, exit) = run(&client, wait()).await;
    assert_eq!(exit, 2);
    assert_eq!(result["verdict"], "stopped");
    assert_eq!(
        server
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["operation"] == "runs/get")
            .count(),
        3
    );
}
#[tokio::test]
async fn deadline_bounds_running_and_hung_reads_without_sending_stop() {
    for mode in [Mode::Normal, Mode::HungRead] {
        let server = FakeHome::start(false).await;
        let client = paired(&server).await;
        server.run.lock().unwrap()["status"] = json!("running");
        server.run.lock().unwrap()["state"] = json!("running");
        server.set(mode);
        let started = tokio::time::Instant::now();
        let (result, exit) = execute_device(&client, wait(), Duration::from_millis(100)).await;
        assert_eq!(exit, 3);
        assert_eq!(result.verdict, "deadline");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            result
                .failure_reason
                .unwrap()
                .contains("task was not cancelled")
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        let seen = server.seen.lock().unwrap();
        assert_eq!(
            seen.iter().filter(|v| v["operation"] == "runs/get").count(),
            1
        );
        assert!(
            !seen
                .iter()
                .any(|v| v["operation"] == "stop" || v["operation"] == "dispatch")
        );
        assert_eq!(server.run.lock().unwrap()["cancelRequested"], false);
    }
}
#[tokio::test]
async fn missing_or_mismatched_answer_is_never_a_pass() {
    for kind in [
        "missing",
        "foreign-run",
        "foreign-thread",
        "empty",
        "non-bot",
    ] {
        let server = FakeHome::start(false).await;
        let client = paired(&server).await;
        match kind {
            "missing" => {
                server.run.lock().unwrap()["messageId"] = Value::Null;
                server.run.lock().unwrap()["failure"] =
                    json!({"category":"other","message":"sensitive-canary provider prose"});
            }
            "foreign-run" => {
                server.messages.lock().unwrap()["messages"][0]["runId"] = json!("foreign")
            }
            "foreign-thread" => server.messages.lock().unwrap()["threadId"] = json!("foreign"),
            "empty" => server.messages.lock().unwrap()["messages"][0]["blocks"] = json!([]),
            "non-bot" => server.messages.lock().unwrap()["messages"][0]["role"] = json!("user"),
            _ => unreachable!(),
        };
        let (result, exit) = run(&client, send(true, "Fixture")).await;
        assert_eq!(exit, 2);
        assert_eq!(result["verdict"], "error");
        assert_eq!(
            result["failureReason"],
            "The task finished, but its answer is unavailable. Open it at home."
        );
    }
}
#[tokio::test]
async fn failed_reads_are_successful_but_waits_fail_and_prose_is_discarded() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    {
        let mut state = server.run.lock().unwrap();
        state["status"] = json!("failed");
        state["state"] = json!("failed");
        state["failure"] = json!({"category":"signed-out","message":"sensitive-canary private provider diagnostic"});
        state["providerError"] = json!("sensitive-canary");
    }
    let (shown, exit) = run(
        &client,
        DeviceCommand::RunsShow {
            run_id: "run".into(),
        },
    )
    .await;
    assert_eq!(exit, 0);
    assert_eq!(shown["data"]["run"]["failure"]["category"], "signed-out");
    let (waited, exit) = run(&client, wait()).await;
    assert_eq!(exit, 2);
    assert_eq!(waited["verdict"], "failed");
    assert!(
        waited["failureReason"]
            .as_str()
            .unwrap()
            .starts_with("Sign in")
    );
    {
        let mut state = server.run.lock().unwrap();
        state["status"] = json!("waiting_input");
        state["state"] = json!("running");
        state["failure"] = Value::Null;
    }
    assert_eq!(run(&client, wait()).await.1, 4);
    server.set(Mode::Revoked);
    assert_eq!(run(&client, wait()).await.1, 4);
}
#[tokio::test]
async fn input_and_protocol_refusals_do_not_echo_sensitive_fields() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    for command in [
        DeviceCommand::RunsList {
            cursor: None,
            limit: 101,
        },
        send(false, " "),
        DeviceCommand::Send {
            bot: "bot".into(),
            text: "x".into(),
            request_id: "short".into(),
            wait: false,
        },
    ] {
        assert_eq!(run(&client, command).await.1, 4);
    }
    {
        let mut state = server.run.lock().unwrap();
        state["runId"] = json!("different");
    }
    assert_eq!(run(&client, wait()).await.1, 2);
    server.set(Mode::SecretError);
    assert_eq!(run(&client, wait()).await.1, 2);
}
#[test]
fn key_material_is_redacted_before_serialization() {
    let result = home_client::safe_output(
        "visible -----BEGIN PRIVATE KEY-----\nsynthetic\n-----END PRIVATE KEY----- Bearer synthetic-token-000000000000",
    );
    assert_eq!(result, "visible [redacted] [redacted]");
}
