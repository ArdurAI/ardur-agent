//! SSE streaming endpoint tests.
//!
//! These tests verify the streaming contract: `stream: true` returns
//! `text/event-stream` instead of `400`.

mod support;

use std::sync::Arc;
use std::time::Duration;

use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, Provider, ProviderError, ProviderId, ProviderStream,
    RateCard, StreamEvent, Usage,
};
use ardur_runtime::CostTuple;
use ardur_server::{AppState, Config, build_router, example_registry};
use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::json;
use tower::ServiceExt;

struct ErroringProvider {
    rate_card: RateCard,
}

#[async_trait]
impl Provider for ErroringProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Err(ProviderError::Upstream(
            "bad upstream: \"quoted\"\nand newline".to_string(),
        ))
    }

    fn id(&self) -> ProviderId {
        ProviderId("erroring".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

struct SlowStreamingProvider {
    rate_card: RateCard,
}

#[async_trait]
impl Provider for SlowStreamingProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Ok(CompletionResponse {
            content: "buffered fallback".to_string(),
            finish_reason: ardur_provider_runtime::FinishReason::Stop,
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
                2 => Some((
                    Ok(StreamEvent::Finish(
                        ardur_provider_runtime::FinishReason::Stop,
                    )),
                    3,
                )),
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

fn sse_payloads(text: &str) -> Vec<serde_json::Value> {
    text.split("\n\n")
        .filter_map(|frame| frame.strip_prefix("data: "))
        .map(|payload| serde_json::from_str(payload).expect("SSE data frame is JSON"))
        .collect()
}

/// Test that `POST /chat` with `stream: true` returns `200` and `text/event-stream`.
#[tokio::test]
async fn streaming_returns_sse_content_type() {
    let router = support::boot_router(&support::test_config(
        Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))),
        None,
    ))
    .await;

    let body = json!({
        "message": "hello",
        "stream": true,
    });
    let request = Request::builder()
        .method("POST")
        .uri("/chat")
        .header(header::CONTENT_TYPE, "application/json")
        .header("authorization", format!("Bearer {}", support::CHAT_TOKEN))
        .body(Body::from(body.to_string()))
        .unwrap();

    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .expect("content-type header present")
        .to_str()
        .unwrap();
    assert!(
        content_type.contains("text/event-stream"),
        "expected text/event-stream, got {content_type}"
    );
}

/// Test that the SSE body contains a `data:` line.
#[tokio::test]
async fn streaming_body_contains_data_event() {
    let router = support::boot_router(&support::test_config(
        Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))),
        None,
    ))
    .await;

    let body = json!({
        "message": "hello",
        "stream": true,
    });
    let request = Request::builder()
        .method("POST")
        .uri("/chat")
        .header(header::CONTENT_TYPE, "application/json")
        .header("authorization", format!("Bearer {}", support::CHAT_TOKEN))
        .body(Body::from(body.to_string()))
        .unwrap();

    let response = router.oneshot(request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(
        text.starts_with("data:"),
        "SSE body should start with 'data:', got: {text}"
    );
    let payloads = sse_payloads(&text);
    assert!(
        payloads.iter().any(|p| p["type"] == "stage_start"),
        "stream should include fused stage events, got: {payloads:?}"
    );
    assert!(
        payloads
            .iter()
            .any(|p| p["type"] == "content" && p["text"] == "[anthropic stub]"),
        "stream should include content deltas, got: {payloads:?}"
    );
    assert!(
        payloads
            .iter()
            .any(|p| { p["type"] == "receipt" && p["cost_cents"].as_u64().is_some() }),
        "receipt events should expose authoritative combined cost, got: {payloads:?}"
    );
    assert!(
        payloads.iter().any(|p| p["type"] == "finish"),
        "stream should include terminal finish, got: {payloads:?}"
    );
}

#[tokio::test]
async fn dropping_sse_response_before_reading_mints_no_receipt() {
    let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
    let config: Config = support::test_config(dir, None);
    let provider: Arc<dyn Provider> = Arc::new(SlowStreamingProvider {
        rate_card: RateCard::anthropic_2026_q2_v1(),
    });
    let tools = Arc::new(example_registry("stub", "in-memory"));
    let state = AppState::boot(&config, provider, tools)
        .await
        .expect("AppState boots");
    let router = build_router(state.clone());

    let request = Request::builder()
        .method("POST")
        .uri("/chat")
        .header(header::CONTENT_TYPE, "application/json")
        .header("authorization", format!("Bearer {}", support::CHAT_TOKEN))
        .body(Body::from(
            json!({ "message": "cancel me", "stream": true }).to_string(),
        ))
        .unwrap();

    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);

    tokio::time::sleep(Duration::from_millis(750)).await;
    assert_eq!(
        state.receipt_count(),
        0,
        "a dropped SSE body cancels the in-flight stream before any receipt is minted"
    );
}

#[tokio::test]
async fn streaming_error_event_is_valid_json() {
    let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
    let config = support::test_config(dir, None);
    let provider: Arc<dyn Provider> = Arc::new(ErroringProvider {
        rate_card: RateCard::anthropic_2026_q2_v1(),
    });
    let tools = Arc::new(example_registry("stub", "in-memory"));
    let state = AppState::boot(&config, provider, tools)
        .await
        .expect("AppState boots");
    let router = build_router(state);

    let request = Request::builder()
        .method("POST")
        .uri("/chat")
        .header(header::CONTENT_TYPE, "application/json")
        .header("authorization", format!("Bearer {}", support::CHAT_TOKEN))
        .body(Body::from(
            json!({ "message": "hello", "stream": true }).to_string(),
        ))
        .unwrap();

    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let parsed = sse_payloads(&text)
        .into_iter()
        .find(|payload| payload["type"] == "error")
        .expect("stream carries an in-band error event");
    assert!(!parsed["error"].as_str().unwrap_or_default().is_empty());
}
