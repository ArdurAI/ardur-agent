//! `chat` — the `POST /chat` synchronous HTTP surface (§4.0b).
//!
//! Drives the real router in-process via `tower::ServiceExt::oneshot` (no
//! socket). The offline-stub cases boot over [`AnthropicProvider::stub`]; the
//! runtime-error, tools-called, and token/cost cases inject a small scripted
//! [`Provider`] so the corresponding pipeline outcomes are exercised
//! deterministically without a live model.

mod support;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, Provider, ProviderError, RateCard, Usage,
};
use ardur_runtime::{CostTuple, ProviderId, ToolCall};
use ardur_server::{AppState, Config, build_router, example_registry};
use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};

/// POST a JSON body to `/chat` on `router`, returning the status and parsed JSON.
async fn post_chat(router: Router, body: Value) -> (StatusCode, Value) {
    post_chat_with_auth(router, body, Some(support::CHAT_TOKEN)).await
}

async fn post_chat_with_auth(
    router: Router,
    body: Value,
    bearer: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/chat")
        .header("content-type", "application/json");
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let request = builder
        .body(Body::from(body.to_string()))
        .expect("request builds");
    let (status, bytes) = support::oneshot(router, request).await;
    let json: Value = serde_json::from_slice(&bytes).expect("response body is JSON");
    (status, json)
}

/// Boot a stub-backed router over a fresh tempdir config.
fn stub_router() -> Router {
    let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
    let config = support::test_config(dir, None);
    support::boot_router(&config)
}

/// Boot a router whose model backend is the scripted `provider`.
fn scripted_router(provider: Arc<dyn Provider>) -> Router {
    let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
    let config: Config = support::test_config(dir, None);
    let tools = Arc::new(example_registry("scripted", "in-memory"));
    let state = AppState::boot(&config, provider, tools).expect("AppState boots");
    build_router(state)
}

// ---------------------------------------------------------------------------
// A scripted provider: returns a queued step per `complete` call, defaulting to
// a terminal stub reply once the script is drained.
// ---------------------------------------------------------------------------

/// One scripted provider response.
enum Step {
    /// A terminal assistant reply carrying `content` and the given billed cost.
    Reply { content: String, cost: CostTuple },
    /// A tool-use turn requesting `calls` (the loop executes them, then asks
    /// again — so a `Reply` must follow in the script).
    ToolUse(Vec<ToolCall>),
    /// A provider failure (surfaces as `RuntimeError` → HTTP 502).
    Error,
}

struct ScriptedProvider {
    rate_card: RateCard,
    steps: Mutex<VecDeque<Step>>,
}

impl ScriptedProvider {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            rate_card: RateCard::anthropic_2026_q2_v1(),
            steps: Mutex::new(steps.into_iter().collect()),
        })
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let step = self.steps.lock().expect("steps lock").pop_front();
        match step {
            Some(Step::Error) => Err(ProviderError::Upstream(
                "scripted provider error".to_string(),
            )),
            Some(Step::ToolUse(calls)) => Ok(CompletionResponse {
                content: String::new(),
                finish_reason: FinishReason::ToolUse(calls),
                usage: Usage {
                    tokens_in: 0,
                    tokens_out: 0,
                    cost_cents: None,
                },
                cost: CostTuple::default(),
                raw_provider_response: None,
            }),
            Some(Step::Reply { content, cost }) => Ok(CompletionResponse {
                content,
                finish_reason: FinishReason::Stop,
                usage: Usage {
                    tokens_in: cost.tokens_in as u32,
                    tokens_out: cost.tokens_out as u32,
                    cost_cents: None,
                },
                cost,
                raw_provider_response: None,
            }),
            None => Ok(CompletionResponse {
                content: "[scripted: done]".to_string(),
                finish_reason: FinishReason::Stop,
                usage: Usage {
                    tokens_in: 0,
                    tokens_out: 0,
                    cost_cents: None,
                },
                cost: CostTuple::default(),
                raw_provider_response: None,
            }),
        }
    }

    fn id(&self) -> ProviderId {
        ProviderId("scripted".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_rejects_missing_bearer_before_body_processing() {
    let (status, json) =
        post_chat_with_auth(stub_router(), json!({ "message": "hello" }), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("bearer")
    );
}

#[tokio::test]
async fn chat_rejects_invalid_bearer() {
    let (status, json) = post_chat_with_auth(
        stub_router(),
        json!({ "message": "hello" }),
        Some("wrong-token"),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("bearer")
    );
}

#[tokio::test]
async fn chat_returns_reply_with_offline_provider() {
    let (status, json) = post_chat(stub_router(), json!({ "message": "hello ardur" })).await;
    assert_eq!(status, StatusCode::OK);
    // The stub provider's deterministic completion is returned to the caller.
    assert_eq!(json["reply"], "[anthropic stub]");
}

#[tokio::test]
async fn chat_generates_session_id_when_missing() {
    let (status, json) = post_chat(stub_router(), json!({ "message": "hi" })).await;
    assert_eq!(status, StatusCode::OK);
    let session_id = json["session_id"].as_str().expect("session_id is a string");
    assert!(!session_id.is_empty(), "a fresh session id was minted");
    // It is a real UUID, not an echo of the (absent) request value.
    assert!(
        uuid_like(session_id),
        "minted session id parses as a UUID: {session_id}"
    );
}

#[tokio::test]
async fn chat_uses_provided_session_id() {
    let provided = "018f5e1a-0000-7000-8000-000000000abc";
    let (status, json) = post_chat(
        stub_router(),
        json!({ "message": "hi", "session_id": provided }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json["session_id"], provided,
        "the provided session id round-trips unchanged"
    );
}

#[tokio::test]
async fn chat_400_on_missing_message() {
    let (status, json) = post_chat(stub_router(), json!({ "session_id": null })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(json["error"].is_string(), "carries a JSON error message");
}

#[tokio::test]
async fn chat_400_on_empty_message() {
    let (status, _json) = post_chat(stub_router(), json!({ "message": "   " })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn chat_400_on_stream_true() {
    // Streaming (SSE) is the P1.5 follow-up; a `stream: true` request is refused
    // rather than silently answered with a consolidated body.
    let (status, _json) =
        post_chat(stub_router(), json!({ "message": "hi", "stream": true })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn chat_502_on_runtime_error() {
    // A provider failure propagates as a `RuntimeError`, which the HTTP surface
    // maps to 502 (the upstream pipeline errored).
    let provider = ScriptedProvider::new(vec![Step::Error]);
    let (status, json) = post_chat(scripted_router(provider), json!({ "message": "boom" })).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        json["error"].is_string(),
        "carries the runtime error message"
    );
}

#[tokio::test]
async fn chat_tokens_and_cost_in_response() {
    // A scripted reply that bills 120 in / 30 out tokens and 250 cents.
    let cost = CostTuple {
        tokens_in: 120,
        tokens_out: 30,
        cents: 250,
        wall_ms: 0,
        attention_score: 0.0,
    };
    let provider = ScriptedProvider::new(vec![Step::Reply {
        content: "scripted reply".to_string(),
        cost,
    }]);
    let (status, json) = post_chat(scripted_router(provider), json!({ "message": "spend" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["tokens"]["input"], 120);
    assert_eq!(json["tokens"]["output"], 30);
    // 250 cents == $2.50.
    assert_eq!(json["cost_usd"], 2.5);
}

#[tokio::test]
async fn chat_tools_called_list_populated() {
    // The model asks for the `echo` tool (registered by `example_registry`), then
    // settles on a final answer. The response lists the tool it invoked.
    let call = ToolCall {
        id: "call_1".to_string(),
        name: "echo".to_string(),
        arguments: json!({ "message": "ping" }),
    };
    let provider = ScriptedProvider::new(vec![
        Step::ToolUse(vec![call]),
        Step::Reply {
            content: "done".to_string(),
            cost: CostTuple::default(),
        },
    ]);
    let (status, json) = post_chat(
        scripted_router(provider),
        json!({ "message": "use a tool" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let tools = json["tools_called"]
        .as_array()
        .expect("tools_called is an array");
    assert_eq!(
        tools,
        &vec![Value::from("echo")],
        "the invoked tool is listed"
    );
}

#[tokio::test]
async fn chat_receipt_id_in_response() {
    let (status, json) = post_chat(stub_router(), json!({ "message": "receipt please" })).await;
    assert_eq!(status, StatusCode::OK);
    let receipt_id = json["receipt_id"].as_str().expect("receipt_id is a string");
    assert!(
        !receipt_id.is_empty(),
        "a receipt id was minted and returned"
    );
    assert!(
        uuid_like(receipt_id),
        "receipt id parses as a UUID: {receipt_id}"
    );
}

/// A loose UUID shape check (8-4-4-4-12 hex groups) — avoids a `uuid` dev-dep.
fn uuid_like(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&groups)
            .all(|(&n, g)| g.len() == n && g.chars().all(|c| c.is_ascii_hexdigit()))
}
