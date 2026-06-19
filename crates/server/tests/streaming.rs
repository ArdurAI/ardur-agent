//! SSE streaming endpoint tests.
//!
//! These tests verify the streaming contract: `stream: true` returns
//! `text/event-stream` instead of `400`.

mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::json;
use tower::ServiceExt;

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
