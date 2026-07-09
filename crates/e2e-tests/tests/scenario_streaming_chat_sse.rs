//! Scenario — streaming `/chat` SSE.
//!
//! Drives the real `ardur-server` router in-process and proves that
//! `POST /chat {"stream": true}` is no longer the old buffered placeholder: the
//! body is a sequence of JSON SSE frames carrying fused-runtime stage/content/
//! usage/receipt/finish events. A second turn drops the response body before
//! reading it and asserts no receipt is minted, preserving the stream
//! cancellation contract.

use std::sync::Arc;
use std::time::Duration;

use ardur_provider_runtime::{
    AnthropicProvider, CompletionRequest, CompletionResponse, FinishReason, ModelId, Provider,
    ProviderError, ProviderId, ProviderStream, RateCard, StreamEvent, Usage,
};
use ardur_runtime::CostTuple;
use ardur_server::{AppState, Config, LogFormat, MemoryBackend, build_router};
use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt as _;

const CHAT_TOKEN: &str = "e2e-streaming-chat-token";

fn test_config(data_dir: &tempfile::TempDir) -> Config {
    Config {
        anthropic_api_key: String::new(),
        slack_bot_token: "redacted-test-token".to_string(),
        slack_signing_secret: "streaming-e2e-signing-secret".to_string(),
        slack_app_id: "A0STREAMINGE2E".to_string(),
        slack_allowed_senders: vec!["U0STREAM".to_string()],
        data_dir: data_dir.path().to_path_buf(),
        bind_addr: "127.0.0.1:0".to_string(),
        chat_bearer_tokens: vec![CHAT_TOKEN.to_string()],
        admin_bearer_tokens: Vec::new(),
        dev_permissive_policy: true,
        model: "claude-opus-4-8".to_string(),
        cost_budget_cents: 10_000,
        cedar_policy_path: None,
        slack_base_url: None,
        channel_matrix: false,
        channel_discord: false,
        channel_telegram: false,
        log_format: LogFormat::Text,
        mcp_enabled: false,
        mcp_bearer_tokens: Vec::new(),
        mcp_path_prefix: "/mcp".to_string(),
        mcp_remote_servers: Vec::new(),
        skills_dirs: Vec::new(),
        memory_backend: MemoryBackend::InMemory,
        qdrant_url: None,
        qdrant_collection: None,
    }
}

fn sse_payloads(text: &str) -> Vec<serde_json::Value> {
    text.split("\n\n")
        .filter_map(|frame| frame.strip_prefix("data: "))
        .map(|payload| serde_json::from_str(payload).expect("SSE data frame is JSON"))
        .collect()
}

fn streaming_request(message: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/chat")
        .header(header::CONTENT_TYPE, "application/json")
        .header("authorization", format!("Bearer {CHAT_TOKEN}"))
        .body(Body::from(
            serde_json::json!({ "message": message, "stream": true }).to_string(),
        ))
        .expect("request builds")
}

#[tokio::test]
async fn scenario_streaming_chat_sse_emits_fused_events_and_receipt() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let config = test_config(&data_dir);
    let provider: Arc<dyn Provider> =
        Arc::new(AnthropicProvider::stub(ModelId::new(&config.model)));
    let tools = Arc::new(ardur_server::example_registry("stub", "in-memory"));
    let state = AppState::boot(&config, provider, tools).expect("server boots");
    let router = build_router(state.clone());

    let response = router
        .oneshot(streaming_request("stream the deployment path"))
        .await
        .expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .expect("content type")
        .to_str()
        .expect("utf8 content type");
    assert!(content_type.contains("text/event-stream"));

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collects");
    let text = String::from_utf8(bytes.to_vec()).expect("utf8 body");
    let payloads = sse_payloads(&text);

    for event_type in [
        "stage_start",
        "stage_end",
        "content",
        "usage",
        "receipt",
        "finish",
    ] {
        assert!(
            payloads.iter().any(|p| p["type"] == event_type),
            "missing {event_type} in SSE payloads: {payloads:?}"
        );
    }
    assert!(
        payloads
            .iter()
            .any(|p| p["type"] == "content" && p["text"] == "[anthropic stub]"),
        "assistant content arrives as a stream delta: {payloads:?}"
    );
    assert_eq!(
        state.receipt_count(),
        1,
        "completed stream minted one receipt"
    );
}

struct SlowStreamingProvider {
    rate_card: RateCard,
}

#[async_trait]
impl Provider for SlowStreamingProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Ok(CompletionResponse {
            content: "buffered fallback".to_string(),
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
            cost: CostTuple::default(),
            raw_provider_response: None,
        })
    }

    async fn stream(&self, _req: CompletionRequest) -> Result<ProviderStream, ProviderError> {
        let events = futures::stream::unfold(0_u8, |state| async move {
            match state {
                0 => Some((Ok(StreamEvent::ContentDelta("partial".to_string())), 1)),
                1 => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    Some((Ok(StreamEvent::Usage(Usage::default())), 2))
                }
                2 => Some((Ok(StreamEvent::Finish(FinishReason::Stop)), 3)),
                _ => None,
            }
        });
        Ok(Box::pin(events))
    }

    fn id(&self) -> ProviderId {
        ProviderId("slow-stream".to_string())
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

#[tokio::test]
async fn scenario_streaming_chat_cancel_drops_before_receipt() {
    let data_dir = tempfile::tempdir().expect("tempdir");
    let config = test_config(&data_dir);
    let provider: Arc<dyn Provider> = Arc::new(SlowStreamingProvider {
        rate_card: RateCard::anthropic_2026_q2_v1(),
    });
    let tools = Arc::new(ardur_server::example_registry("stub", "in-memory"));
    let state = AppState::boot(&config, provider, tools).expect("server boots");
    let router = build_router(state.clone());

    let response = router
        .oneshot(streaming_request("cancel before completion"))
        .await
        .expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);

    tokio::time::sleep(Duration::from_millis(750)).await;
    assert_eq!(
        state.receipt_count(),
        0,
        "cancelled stream leaves no orphan or completed receipt"
    );
}
