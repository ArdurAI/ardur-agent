//! Scenario §4.1 — `server_slack_round_trip`.
//!
//! The full deployment loop, end to end, without a live Slack workspace: a
//! genuinely HMAC-signed Slack `message` event is POSTed to the real
//! [`ardur_server`] axum router (driven in-process via
//! `tower::ServiceExt::oneshot`), routed through the real [`AppState`] →
//! [`FusedRuntime`](ardur_fused_runtime::FusedRuntime) pipeline over a stub
//! provider, and the assistant's reply is observed landing on a wiremock
//! `chat.postMessage`.
//!
//! Unlike the per-crate suite in `crates/server/tests`, this scenario lives in
//! the cross-crate host — proving the binary's wiring composes with the rest of
//! the substrate exactly as deployed.

use std::sync::Arc;
use std::time::Duration;

use ardur_provider_runtime::{AnthropicProvider, ModelId, Provider};
use ardur_server::{AppState, Config, LogFormat, MemoryBackend, build_router};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use tower::ServiceExt as _;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

type HmacSha256 = Hmac<Sha256>;

const BOT_TOKEN: &str = "xoxb-e2e-server-token";
const SIGNING_SECRET: &str = "e2e-server-signing-secret-0000abcd";
const APP_ID: &str = "A0E2ESERVER";

/// Recompute the genuine Slack `v0=<hex>` request signature.
fn sign(timestamp: &str, body: &str) -> String {
    let basestring = format!("v0:{timestamp}:{body}");
    let mut mac =
        HmacSha256::new_from_slice(SIGNING_SECRET.as_bytes()).expect("hmac accepts any key length");
    mac.update(basestring.as_bytes());
    format!("v0={}", hex::encode(mac.finalize().into_bytes()))
}

/// Current Unix seconds as a string — a fresh, non-replayed request timestamp.
fn now_unix_string() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
        .to_string()
}

#[tokio::test]
async fn server_routes_signed_slack_message_through_runtime_to_chat_post_message() {
    // Mock Slack's outbound `chat.postMessage`.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "ts": "1700000000.000900",
            "channel": "C0DEPLOY"
        })))
        .mount(&server)
        .await;

    // Boot the *real* server state over a stub provider + tempdir.
    let data_dir = tempfile::tempdir().expect("tempdir");
    let config = Config {
        anthropic_api_key: String::new(),
        slack_bot_token: BOT_TOKEN.to_string(),
        slack_signing_secret: SIGNING_SECRET.to_string(),
        slack_app_id: APP_ID.to_string(),
        data_dir: data_dir.path().to_path_buf(),
        bind_addr: "127.0.0.1:0".to_string(),
        model: "claude-opus-4-8".to_string(),
        cost_budget_cents: 10_000,
        cedar_policy_path: None,
        slack_base_url: Some(server.uri()),
        channel_matrix: false,
        log_format: LogFormat::Text,
        mcp_enabled: false,
        mcp_bearer_tokens: Vec::new(),
        mcp_path_prefix: "/mcp".to_string(),
        mcp_remote_servers: Vec::new(),
        memory_backend: MemoryBackend::InMemory,
        qdrant_url: None,
    };
    let provider: Arc<dyn Provider> =
        Arc::new(AnthropicProvider::stub(ModelId::new(&config.model)));
    let state = AppState::boot(&config, provider).expect("the server boots");
    let router = build_router(state);

    // A genuine, signed inbound user message.
    let ts = now_unix_string();
    let body = serde_json::json!({
        "type": "event_callback",
        "event": {
            "type": "message",
            "user": "U0DEPLOY",
            "text": "is the deployment loop alive?",
            "channel": "C0DEPLOY",
            "ts": "1700000000.000100"
        }
    })
    .to_string();
    let signature = sign(&ts, &body);

    let request = Request::builder()
        .method("POST")
        .uri("/slack/events")
        .header("X-Slack-Signature", signature)
        .header("X-Slack-Request-Timestamp", ts)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("request builds");

    // The webhook acks immediately; the worker processes + posts asynchronously.
    let response = router.oneshot(request).await.expect("router responds");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the webhook acks the event"
    );

    // The reply lands on the mocked Slack within the deadline.
    let sent = wait_for_post(&server, Duration::from_secs(10)).await;
    assert_eq!(sent.len(), 1, "exactly one reply was posted");

    let posted: serde_json::Value =
        serde_json::from_slice(&sent[0].body).expect("posted body is JSON");
    assert_eq!(
        posted["channel"], "C0DEPLOY",
        "reply targets the source channel"
    );
    assert_eq!(
        posted["text"], "[anthropic stub]",
        "the runtime's response crosses the wire as the reply"
    );

    // Boot persisted the receipt chain + journal — the deployment is durable.
    assert!(
        data_dir.path().join("receipts/chain.jsonl").is_file(),
        "the turn's receipt was persisted"
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
