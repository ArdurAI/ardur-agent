//! `slack_message_round_trip` — a signed Slack `message` event is verified,
//! processed through the fused runtime (stub provider), and the reply is posted
//! back via `chat.postMessage` (mocked by wiremock). Proves the full deployment
//! loop without a live Slack workspace.

mod support;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn signed_message_runs_through_runtime_and_posts_reply() {
    // Mock Slack's `chat.postMessage`.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "ts": "1700000000.000300",
            "channel": "C99999"
        })))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, Some(server.uri()));
    let router = support::boot_router(&config);

    // A genuine, signed inbound user message.
    let ts = support::now_unix_string();
    let body = serde_json::json!({
        "type": "event_callback",
        "event": {
            "type": "message",
            "user": "U4242",
            "text": "hello ardur",
            "channel": "C99999",
            "ts": "1700000000.000100"
        }
    })
    .to_string();
    let signature = support::sign(&ts, &body);

    let request = Request::builder()
        .method("POST")
        .uri("/slack/events")
        .header("X-Slack-Signature", signature)
        .header("X-Slack-Request-Timestamp", ts)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("request builds");

    // The webhook acks immediately; the worker processes + posts asynchronously.
    let (status, _ack) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK, "the webhook acks the event");

    // Wait for the worker's `chat.postMessage` to land on the mock.
    let sent = wait_for_post(&server, Duration::from_secs(10)).await;
    assert_eq!(sent.len(), 1, "exactly one reply was posted");

    let posted: serde_json::Value =
        serde_json::from_slice(&sent[0].body).expect("posted body is JSON");
    assert_eq!(
        posted["channel"], "C99999",
        "reply targets the source channel"
    );
    // The stub provider's deterministic completion is echoed back to Slack.
    assert_eq!(
        posted["text"], "[anthropic stub]",
        "the runtime's response is posted as the reply text"
    );

    // The reply carried the bot-token bearer auth.
    assert_eq!(
        sent[0]
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap_or_default())
            .unwrap_or_default(),
        format!("Bearer {}", support::BOT_TOKEN)
    );
}

/// Poll the mock until it has recorded at least one request, or `timeout`
/// elapses (panicking — the worker should always post within it).
async fn wait_for_post(server: &MockServer, timeout: Duration) -> Vec<wiremock::Request> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(requests) = server.received_requests().await {
            if !requests.is_empty() {
                return requests;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for the worker to POST chat.postMessage");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
