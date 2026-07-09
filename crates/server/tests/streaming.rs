//! SSE streaming endpoint tests.
//!
//! These tests verify the streaming contract: `stream: true` returns
//! `text/event-stream` instead of `400`.

mod support;

use std::sync::Arc;

use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, Provider, ProviderError, ProviderId, RateCard,
};
use ardur_server::{AppState, build_router, example_registry};
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

/// Test that `POST /chat` with `stream: true` returns `200` and `text/event-stream`.
#[tokio::test]
async fn streaming_returns_sse_content_type() {
    let router = support::boot_router(&support::test_config(
        Box::leak(Box::new(tempfile::tempdir().expect("tempdir"))),
        None,
    ));

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
    ));

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
}

#[tokio::test]
async fn streaming_error_event_is_valid_json() {
    let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
    let config = support::test_config(dir, None);
    let provider: Arc<dyn Provider> = Arc::new(ErroringProvider {
        rate_card: RateCard::anthropic_2026_q2_v1(),
    });
    let tools = Arc::new(example_registry("stub", "in-memory"));
    let state = AppState::boot(&config, provider, tools).expect("AppState boots");
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
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let payload = text
        .strip_prefix("data: ")
        .and_then(|s| s.strip_suffix("\n\n"))
        .expect("SSE data frame shape");
    let parsed: serde_json::Value = serde_json::from_str(payload).expect("valid JSON payload");
    assert!(!parsed["error"].as_str().unwrap_or_default().is_empty());
}
