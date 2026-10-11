mod support;
use home_client::{Error, HomeClient, execute_run_events, pair_device};
use serde_json::{Value, json};
use support::{EventResponse, FakeHome, Mode};

fn event(seq: i64) -> String {
    format!(
        "id: {seq}\nevent: event\ndata: {}\n\n",
        json!({"seq":seq,"runId":"run","threadId":"thread","botId":"bot","type":"thread.progress","payload":{"text":"Reply ✓ 🚀"}})
    )
}
fn window(cursor: i64, reason: &str) -> String {
    format!(
        "event: window\ndata: {}\n\n",
        json!({"nextCursor":cursor,"reason":reason})
    )
}
fn enqueue(home: &FakeHome, text: String, interrupted: bool, finish_run: bool) {
    // One-byte writes deliberately include splits inside Unicode and frame boundaries.
    home.event_windows.lock().unwrap().push_back(EventResponse {
        chunks: text.bytes().map(|b| vec![b]).collect(),
        interrupted,
        finish_run,
    });
}
async fn paired(home: &FakeHome) -> HomeClient {
    HomeClient::new(pair_device(&home.code(), "Fixture").await.unwrap()).unwrap()
}
async fn collect(
    client: &HomeClient,
    cursor: i64,
    follow: bool,
) -> (Result<home_client::EventWindow, Error>, Vec<Value>) {
    let mut records = Vec::new();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        execute_run_events(client, "run", cursor, follow, |record| {
            records.push(record.json());
            Ok(())
        }),
    )
    .await
    .expect("bounded event test");
    (result, records)
}
fn sequences(records: &[Value]) -> Vec<i64> {
    records
        .iter()
        .filter(|r| r["event"] == "event")
        .map(|r| r["cursor"].as_i64().unwrap())
        .collect()
}
#[tokio::test]
async fn one_window_streams_unicode_events_and_preserves_thread_gaps() {
    let home = FakeHome::start(false).await;
    let client = paired(&home).await;
    enqueue(
        &home,
        event(1) + ": heartbeat\n\n" + &event(4) + &window(7, "timeout"),
        false,
        false,
    );
    let (result, records) = collect(&client, -1, false).await;
    assert_eq!(result.unwrap().next_cursor, 7);
    assert_eq!(sequences(&records), vec![1, 4]);
    assert_eq!(records[0]["data"]["payload"]["text"], "Reply ✓ 🚀");
    let seen = home.seen.lock().unwrap();
    let request = seen.iter().find(|r| r["operation"] == "events").unwrap();
    assert_eq!(
        request["body"],
        json!({"runId":"run","threadId":"thread","botId":"bot","cursor":-1})
    );
}
#[tokio::test]
async fn follow_uses_final_cursors_fresh_nonces_and_drains_ended_runs() {
    let home = FakeHome::start(false).await;
    let client = paired(&home).await;
    enqueue(
        &home,
        event(1) + &event(4) + &window(7, "limit"),
        false,
        false,
    );
    enqueue(
        &home,
        event(4) + &event(8) + &window(9, "limit"),
        false,
        false,
    );
    enqueue(&home, event(12) + &window(12, "timeout"), false, true);
    let (result, records) = collect(&client, -1, true).await;
    assert_eq!(result.unwrap().reason, "completed");
    assert_eq!(sequences(&records), vec![1, 4, 8, 12]);
    let seen = home.seen.lock().unwrap();
    let requests: Vec<_> = seen.iter().filter(|r| r["operation"] == "events").collect();
    assert_eq!(
        requests
            .iter()
            .map(|r| r["body"]["cursor"].clone())
            .collect::<Vec<_>>(),
        vec![json!(-1), json!(7), json!(9)]
    );
    let nonces: std::collections::HashSet<_> = requests
        .iter()
        .map(|r| r["proof"]["nonce"].as_str().unwrap())
        .collect();
    assert_eq!(nonces.len(), 3);
    assert_ne!(
        requests[0]["proof"]["signature"],
        requests[1]["proof"]["signature"]
    );
}
#[tokio::test]
async fn heartbeat_only_window_keeps_the_cursor_without_printing_heartbeats() {
    let home = FakeHome::start(false).await;
    let client = paired(&home).await;
    enqueue(
        &home,
        ": heartbeat\n\n: heartbeat\n\n".to_owned() + &window(10, "timeout"),
        false,
        false,
    );
    let (result, records) = collect(&client, 10, false).await;
    assert_eq!(result.unwrap().next_cursor, 10);
    assert_eq!(records.len(), 1);
    assert!(sequences(&records).is_empty());
}
#[tokio::test]
async fn oversized_frame_and_stop_reasons_never_reconnect() {
    for reason in [
        "payload_too_large",
        "access_lost",
        "error",
        "shutdown",
        "oversize",
    ] {
        let home = FakeHome::start(false).await;
        let client = paired(&home).await;
        let text = if reason == "oversize" {
            "a".repeat(65537)
        } else {
            window(4, reason)
        };
        // Use one bulk chunk for the large-frame case.
        home.event_windows.lock().unwrap().push_back(EventResponse {
            chunks: vec![text.into_bytes()],
            interrupted: false,
            finish_run: false,
        });
        let (result, records) = collect(&client, 4, true).await;
        if reason == "oversize" {
            assert_eq!(result.unwrap_err(), Error::PayloadTooLarge);
        } else {
            let result = result.unwrap();
            assert_eq!(result.reason, reason);
            assert!(result.exit_code() != 0);
            assert_eq!(records.len(), 1);
            assert!(records[0]["message"].as_str().unwrap().contains("stopped"));
        }
        assert_eq!(
            home.seen
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r["operation"] == "events")
                .count(),
            1
        );
    }
}
#[tokio::test]
async fn revoked_grant_refuses_before_opening_a_stream() {
    let home = FakeHome::start(false).await;
    let client = paired(&home).await;
    home.set(Mode::Revoked);
    let (result, records) = collect(&client, -1, true).await;
    assert_eq!(result.unwrap_err(), Error::Access);
    assert!(records.is_empty());
    assert!(Error::Access.to_string().contains("permissions"));
}
#[tokio::test]
async fn disconnect_mid_frame_resumes_from_last_complete_event() {
    let home = FakeHome::start(false).await;
    let client = paired(&home).await;
    enqueue(
        &home,
        event(1) + "id: 4\nevent: event\ndata: {\"seq\":4",
        true,
        false,
    );
    enqueue(
        &home,
        event(1) + &event(4) + &window(4, "timeout"),
        false,
        true,
    );
    let (result, records) = collect(&client, -1, true).await;
    assert_eq!(result.unwrap().next_cursor, 4);
    assert_eq!(sequences(&records), vec![1, 4]);
    let seen = home.seen.lock().unwrap();
    let requests: Vec<_> = seen.iter().filter(|r| r["operation"] == "events").collect();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1]["body"]["cursor"], 1);
    assert_ne!(requests[0]["proof"]["nonce"], requests[1]["proof"]["nonce"]);
}
#[tokio::test]
async fn room_run_uses_its_exact_group_and_thread() {
    let home = FakeHome::start(false).await;
    let client = paired(&home).await;
    home.run.lock().unwrap()["groupId"] = json!("room");
    enqueue(&home, window(-1, "timeout"), false, false);
    assert!(collect(&client, -1, false).await.0.is_ok());
    let seen = home.seen.lock().unwrap();
    let request = seen.iter().find(|r| r["operation"] == "events").unwrap();
    assert_eq!(
        request["body"],
        json!({"runId":"run","groupId":"room","threadId":"thread","cursor":-1})
    );
}
#[tokio::test]
async fn uncorrelated_events_and_bad_cursors_are_refused() {
    let home = FakeHome::start(false).await;
    let client = paired(&home).await;
    for cursor in [-2, 2147483648] {
        assert_eq!(
            collect(&client, cursor, true).await.0.unwrap_err(),
            Error::Input
        );
    }
    enqueue(
        &home,
        event(1).replace("\"run\"", "\"another-run\""),
        false,
        false,
    );
    let (result, records) = collect(&client, -1, true).await;
    assert_eq!(result.unwrap_err(), Error::Protocol);
    assert!(records.is_empty());
}
#[tokio::test]
async fn caller_can_cancel_a_follow_during_backoff() {
    let home = FakeHome::start(false).await;
    let client = paired(&home).await;
    home.run.lock().unwrap()["status"] = json!("running");
    enqueue(&home, window(-1, "timeout"), false, false);
    let (sent, received) = tokio::sync::oneshot::channel();
    let mut sent = Some(sent);
    let work = execute_run_events(&client, "run", -1, true, |record| {
        if let Some(sent) = sent.take() {
            sent.send(record.cursor()).unwrap();
        }
        Ok(())
    });
    tokio::select! {
        result = work => panic!("follow ended before cancellation: {result:?}"),
        cursor = received => assert_eq!(cursor.unwrap(), -1),
        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => panic!("event window did not arrive"),
    }
    assert_eq!(
        home.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r["operation"] == "events")
            .count(),
        1
    );
}
