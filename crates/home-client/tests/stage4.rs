mod support;
use home_client::{
    DeviceCommand, FileStore, HomeClient, SecretStore, execute_device, execute_room_send,
    pair_device,
};
use serde_json::{Value, json};
use std::time::Duration;
use support::{FakeHome, Mode};

async fn paired(server: &FakeHome) -> HomeClient {
    HomeClient::new(pair_device(&server.code(), "Fixture").await.unwrap()).unwrap()
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
fn rooms_send(
    group_id: Option<&str>,
    room_name: Option<&str>,
    thread_id: Option<&str>,
    text: &str,
    client_nonce: &str,
) -> DeviceCommand {
    DeviceCommand::RoomsSend {
        group_id: group_id.map(str::to_owned),
        room_name: room_name.map(str::to_owned),
        thread_id: thread_id.map(str::to_owned),
        text: text.into(),
        client_nonce: client_nonce.into(),
    }
}
const WORK_FIXTURE: &str = "fixture-room-request-1";
const NAME_FIXTURE: &str = "fixture-room-request-2";

#[tokio::test]
async fn stage4_commands_sign_requests_matching_the_copied_fixture_vectors() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    let (computers, exit) = run(&client, DeviceCommand::ComputersList).await;
    assert_eq!(exit, 0);
    assert_eq!(computers["data"]["computers"][0]["botId"], "fixture-bot");
    assert_eq!(
        computers["data"]["computers"][0]["status"]["computerId"],
        "fixture-shared-computer"
    );
    assert_eq!(
        computers["data"]["computers"][0]["status"]["screenWidth"],
        1280
    );
    let (listed, exit) = run(
        &client,
        DeviceCommand::BoardList {
            workspace: "fixture-board".into(),
            filter: Some(json!({"status":"open"})),
            search: Some("Fixture".into()),
        },
    )
    .await;
    assert_eq!(exit, 0);
    assert_eq!(listed["data"]["items"][0]["id"], "work-1");
    assert_eq!(listed["data"]["readyIds"], json!(["work-1"]));
    assert_eq!(listed["data"]["blockedIds"], json!([]));
    let (shown, exit) = run(
        &client,
        DeviceCommand::BoardShow {
            workspace: "fixture-board".into(),
            item: "work-1".into(),
        },
    )
    .await;
    assert_eq!(exit, 0);
    assert_eq!(shown["data"]["item"]["title"], "Fixture task");
    let (rooms, exit) = run(&client, DeviceCommand::RoomsList).await;
    assert_eq!(exit, 0);
    assert_eq!(rooms["data"]["rooms"][0]["id"], "fixture-room");
    assert_eq!(rooms["data"]["rooms"][0]["members"][0]["name"], "Chief");
    // Name and id sends reuse the fixture bodies, including Unicode text.
    let (by_name, exit) = run(
        &client,
        rooms_send(None, Some("Fixture room"), None, "hello", NAME_FIXTURE),
    )
    .await;
    assert_eq!(exit, 0);
    assert_eq!(by_name["taskId"], "fixture-task");
    assert_eq!(by_name["runId"], "fixture-run-1");
    assert_eq!(
        by_name["data"]["runIds"],
        json!(["fixture-run-1", "fixture-run-2"])
    );
    let (by_id, exit) = run(
        &client,
        rooms_send(
            Some("fixture-room"),
            None,
            Some("fixture-thread"),
            "@Beta @Gamma compare ✓ 🚀",
            WORK_FIXTURE,
        ),
    )
    .await;
    assert_eq!(exit, 0);
    assert_eq!(by_id["taskId"], "fixture-task");
    let seen = server.seen.lock().unwrap();
    assert!(seen.iter().any(|v| v["operation"] == "rpc"
        && v["body"] == json!({"procedure":"computer/list","input":null})));
    assert!(seen.iter().any(|v| v["operation"] == "rpc"
        && v["body"]
            == json!({"procedure":"board/snapshot","input":{"workspaceId":"fixture-board","filter":{"status":"open"},"search":"Fixture"}})));
    assert!(seen.iter().any(|v| v["operation"] == "rpc"
        && v["body"]
            == json!({"procedure":"board/show","input":{"workspaceId":"fixture-board","id":"work-1"}})));
    assert!(
        seen.iter()
            .any(|v| v["operation"] == "rooms/list" && v["body"] == json!({}))
    );
    assert!(seen.iter().any(|v| v["operation"] == "rooms/send"
        && v["body"]
            == json!({"clientNonce":NAME_FIXTURE,"roomName":"Fixture room","text":"hello"})));
    assert!(seen.iter().any(|v| v["operation"] == "rooms/send"
        && v["body"]
            == json!({"clientNonce":WORK_FIXTURE,"groupId":"fixture-room","text":"@Beta @Gamma compare ✓ 🚀","threadId":"fixture-thread"})));
}

#[tokio::test]
async fn human_output_lists_computers_boards_rooms_and_every_run_id() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    let (result, exit) = execute_device(
        &client,
        DeviceCommand::ComputersList,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(exit, 0);
    assert_eq!(result.human(), "fixture-bot\tFixture bot\tdocker\tstopped");
    let (result, exit) = execute_device(
        &client,
        DeviceCommand::BoardList {
            workspace: "fixture-board".into(),
            filter: None,
            search: None,
        },
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(exit, 0);
    assert_eq!(result.human(), "work-1\topen\tFixture task");
    let (result, exit) = execute_device(
        &client,
        DeviceCommand::BoardShow {
            workspace: "fixture-board".into(),
            item: "work-1".into(),
        },
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(exit, 0);
    assert_eq!(result.human(), "work-1\topen\tp2\tFixture task");
    let (result, exit) =
        execute_device(&client, DeviceCommand::RoomsList, Duration::from_secs(5)).await;
    assert_eq!(exit, 0);
    assert_eq!(result.human(), "fixture-room\tFixture room");
    let (result, exit) = execute_device(
        &client,
        rooms_send(None, Some("Fixture room"), None, "hello", NAME_FIXTURE),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(exit, 0);
    assert_eq!(
        result.human(),
        "Task fixture-task\nRuns fixture-run-1, fixture-run-2"
    );
}

#[tokio::test]
async fn board_denials_and_known_problems_answer_with_the_home_sentences() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    let list = || DeviceCommand::BoardList {
        workspace: "fixture-board".into(),
        filter: None,
        search: None,
    };
    server.set(Mode::BoardDenied);
    let (denied, exit) = run(&client, list()).await;
    assert_eq!(exit, 4);
    assert_eq!(denied["verdict"], "error");
    assert_eq!(
        denied["failureReason"],
        "This board is only available to this computer's owner."
    );
    let (denied, exit) = run(
        &client,
        DeviceCommand::BoardShow {
            workspace: "fixture-board".into(),
            item: "work-1".into(),
        },
    )
    .await;
    assert_eq!(exit, 4);
    assert_eq!(
        denied["failureReason"],
        "This board is only available to this computer's owner."
    );
    server.set(Mode::BoardProblem);
    let (problem, exit) = run(&client, list()).await;
    assert_eq!(exit, 4);
    assert_eq!(problem["failureReason"], "This folder has no board");
    let (problem, exit) = run(
        &client,
        DeviceCommand::BoardShow {
            workspace: "fixture-board".into(),
            item: "work-1".into(),
        },
    )
    .await;
    assert_eq!(exit, 4);
    assert_eq!(problem["failureReason"], "This folder has no board");
}

#[tokio::test]
async fn scope_refusals_keep_the_fixed_access_sentence() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    server.set(Mode::RoomSendRefused);
    let (refused, exit) = run(
        &client,
        rooms_send(None, Some("Fixture room"), None, "hello", NAME_FIXTURE),
    )
    .await;
    assert_eq!(exit, 4);
    assert_eq!(
        refused["failureReason"],
        "This action is unavailable from this device. Check permissions at home."
    );
    server.set(Mode::RecordUnavailable);
    for (output, exit) in [
        run(&client, DeviceCommand::RoomsList).await,
        run(&client, DeviceCommand::ComputersList).await,
        run(
            &client,
            DeviceCommand::BoardList {
                workspace: "fixture-board".into(),
                filter: None,
                search: None,
            },
        )
        .await,
    ] {
        assert_eq!(exit, 4);
        assert_eq!(
            output["failureReason"],
            "This action is unavailable from this device. Check permissions at home."
        );
    }
    server.set(Mode::Revoked);
    assert_eq!(run(&client, DeviceCommand::RoomsList).await.1, 4);
}

#[tokio::test]
async fn ambiguous_or_unknown_room_names_refuse_with_the_home_answer() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    // Two rooms share the name: the home refuses with its fixed answer.
    server.rooms.lock().unwrap().as_array_mut().unwrap().push(
        json!({"id":"fixture-room-2","spaceId":"fixture-space","name":"fixture room","pinned":false,"sectionId":null,"archivedAt":null,"threadId":"fixture-thread-2","preview":"","unread":false,"members":[],"updatedAt":"2026-10-01T00:00:00.000Z","createdAt":"2026-10-01T00:00:00.000Z"}),
    );
    let (ambiguous, exit) = run(
        &client,
        rooms_send(None, Some("Fixture Room"), None, "hello", NAME_FIXTURE),
    )
    .await;
    assert_eq!(exit, 4);
    assert_eq!(
        ambiguous["failureReason"],
        "This record is unavailable from this device."
    );
    // No room matches the name: the same home answer.
    server.rooms.lock().unwrap().as_array_mut().unwrap()[0]["name"] = json!("Elsewhere");
    server.rooms.lock().unwrap().as_array_mut().unwrap()[1]["name"] = json!("Elsewhere");
    let (missing, exit) = run(
        &client,
        rooms_send(None, Some("Fixture room"), None, "hello", NAME_FIXTURE),
    )
    .await;
    assert_eq!(exit, 4);
    assert_eq!(
        missing["failureReason"],
        "This record is unavailable from this device."
    );
    // An exact id and a wrong thread id are checked independently.
    assert_eq!(
        run(
            &client,
            rooms_send(Some("fixture-room"), None, None, "hello", NAME_FIXTURE)
        )
        .await
        .1,
        0
    );
    let (wrong_thread, exit) = run(
        &client,
        rooms_send(
            Some("fixture-room"),
            None,
            Some("foreign-thread"),
            "hello",
            NAME_FIXTURE,
        ),
    )
    .await;
    assert_eq!(exit, 4);
    assert_eq!(
        wrong_thread["failureReason"],
        "This action is unavailable from this device. Check permissions at home."
    );
}

#[tokio::test]
async fn receipt_only_room_response_prints_the_greeting_without_a_run() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    *server.room_result.lock().unwrap() = json!({"kind":"receipt-only","seq":2,"receipt":{"id":"fixture-receipt","threadId":"fixture-thread","seq":3,"botId":"fixture-chief","requestMessageId":"fixture-message","key":"greeting","text":"Hello.","createdAt":"2026-10-01T00:00:00.000Z"}});
    let (result, exit) = execute_device(
        &client,
        rooms_send(None, Some("Fixture room"), None, "hello", NAME_FIXTURE),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(exit, 0);
    let output = result.json();
    assert_eq!(output["data"]["kind"], "receipt-only");
    assert!(output["data"].get("taskId").is_none());
    assert!(output["data"].get("runId").is_none());
    assert_eq!(output["runId"], Value::Null);
    assert_eq!(output["taskId"], Value::Null);
    assert_eq!(output["replyText"], "Hello.");
    assert_eq!(result.human(), "Hello.");
}

#[tokio::test]
async fn lost_room_response_recovers_the_original_admission_with_fresh_proofs() {
    let server = FakeHome::start(false).await;
    let client = paired(&server).await;
    server.set(Mode::LostRoomAdmission);
    let command = rooms_send(None, Some("Fixture room"), None, "hello", WORK_FIXTURE);
    assert_eq!(run(&client, command.clone()).await.1, 2);
    assert_eq!(server.room_admissions.lock().unwrap().len(), 1);
    assert_eq!(
        server
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["operation"] == "rooms/send")
            .count(),
        1
    );
    server.set(Mode::Normal);
    let (recovered, exit) = run(&client, command).await;
    assert_eq!(exit, 0);
    assert_eq!(recovered["taskId"], "fixture-task");
    // Replaying the same nonce with changed text is refused, never merged.
    assert_eq!(
        run(
            &client,
            rooms_send(None, Some("Fixture room"), None, "changed", WORK_FIXTURE)
        )
        .await
        .1,
        4
    );
    let seen = server.seen.lock().unwrap();
    let sends: Vec<_> = seen
        .iter()
        .filter(|v| v["operation"] == "rooms/send")
        .collect();
    assert_eq!(sends.len(), 3);
    assert_eq!(sends[0]["body"], sends[1]["body"]);
    assert_ne!(sends[0]["proof"]["nonce"], sends[1]["proof"]["nonce"]);
    assert_ne!(
        sends[0]["proof"]["signature"],
        sends[1]["proof"]["signature"]
    );
}

#[tokio::test]
async fn room_send_recovery_reuses_one_pending_nonce_for_the_identical_send() {
    let server = FakeHome::start(false).await;
    let stored = pair_device(&server.code(), "Fixture").await.unwrap();
    let client = HomeClient::new(stored.clone()).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("ardur");
    let store = FileStore::new(root);
    store.save(&stored).unwrap();
    let identical = || {
        (
            None,
            Some("Fixture room".to_owned()),
            None,
            "hello".to_owned(),
        )
    };
    // A completed send leaves no recovery record behind.
    let (result, exit) = execute_room_send(
        &store,
        &client,
        None,
        Some("Fixture room".into()),
        None,
        "hello".into(),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(exit, 0);
    assert_eq!(result.json()["taskId"], "fixture-task");
    assert!(store.load_room_send().unwrap().is_none());
    // A lost response records the nonce; the identical rerun reuses it.
    server.set(Mode::LostRoomAdmission);
    let args = identical();
    let (_, exit) = execute_room_send(
        &store,
        &client,
        args.0,
        args.1,
        args.2,
        args.3,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(exit, 2);
    let pending = store.load_room_send().unwrap().expect("pending record");
    assert_eq!(pending.schema_version, 1);
    assert_eq!(pending.room_name.as_deref(), Some("Fixture room"));
    assert_eq!(pending.text, "hello");
    server.set(Mode::Normal);
    let args = identical();
    let (recovered, exit) = execute_room_send(
        &store,
        &client,
        args.0,
        args.1,
        args.2,
        args.3,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(exit, 0);
    assert_eq!(recovered.json()["taskId"], "fixture-task");
    assert!(store.load_room_send().unwrap().is_none());
    // A different message is a new send with its own fresh clientNonce.
    let (_, exit) = execute_room_send(
        &store,
        &client,
        None,
        Some("Fixture room".into()),
        None,
        "different".into(),
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(exit, 0);
    let nonces: Vec<Value> = server
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|v| v["operation"] == "rooms/send")
        .map(|v| v["body"]["clientNonce"].clone())
        .collect();
    assert_eq!(nonces.len(), 4);
    assert_ne!(nonces[0], nonces[1], "a new send gets a fresh clientNonce");
    assert_eq!(
        nonces[1], nonces[2],
        "the identical rerun reuses the pending clientNonce"
    );
    assert_ne!(nonces[1], nonces[3], "a different message is a new send");
    assert!(
        store.load_room_send().unwrap().is_none(),
        "completed send clears the record"
    );
}
