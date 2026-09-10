//! Integration test for the inbound webhook surface: drives the real
//! `receive_webhook` axum handler behind a genuine `axum::Router`, the way an
//! operator's HTTP server would mount it — not just the pure HMAC helpers
//! (already covered by `tests/integration_tests.rs`).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ardur_webhook::{InboundState, WebhookConfig, WebhookError, WebhookEvent, receive_webhook};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use tower::ServiceExt as _;

struct RecordingHandler {
    calls: AtomicUsize,
    last_payload: std::sync::Mutex<Option<serde_json::Value>>,
}

#[async_trait]
impl ardur_webhook::InboundWebhookHandler for RecordingHandler {
    async fn handle(&self, event: WebhookEvent) -> Result<(), WebhookError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_payload.lock().unwrap() = Some(event.payload);
        Ok(())
    }
}

fn router(state: Arc<InboundState>) -> Router {
    Router::new()
        .route("/webhook", post(receive_webhook))
        .with_state(state)
}

#[tokio::test]
async fn a_correctly_signed_request_reaches_the_handler_and_gets_200() {
    let config = WebhookConfig::new("integration-secret", "ci");
    let handler = Arc::new(RecordingHandler {
        calls: AtomicUsize::new(0),
        last_payload: std::sync::Mutex::new(None),
    });
    let state = Arc::new(InboundState {
        config: config.clone(),
        handler: handler.clone(),
    });

    let body = br#"{"status":"deployed"}"#;
    let signature = ardur_webhook::sign_body(body, &config.secret).unwrap();

    let request = Request::builder()
        .method("POST")
        .uri("/webhook")
        .header("x-webhook-signature", signature)
        .header("content-type", "application/json")
        .body(Body::from(body.as_slice()))
        .unwrap();

    let response = router(state).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *handler.last_payload.lock().unwrap(),
        Some(serde_json::json!({"status": "deployed"}))
    );
}

#[tokio::test]
async fn a_tampered_body_is_rejected_before_the_handler_runs() {
    let config = WebhookConfig::new("integration-secret", "ci");
    let handler = Arc::new(RecordingHandler {
        calls: AtomicUsize::new(0),
        last_payload: std::sync::Mutex::new(None),
    });
    let state = Arc::new(InboundState {
        config: config.clone(),
        handler: handler.clone(),
    });

    // Sign one body, send a different one.
    let signature = ardur_webhook::sign_body(b"original", &config.secret).unwrap();
    let request = Request::builder()
        .method("POST")
        .uri("/webhook")
        .header("x-webhook-signature", signature)
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let response = router(state).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        handler.calls.load(Ordering::SeqCst),
        0,
        "the handler must not run on a bad signature"
    );
}

#[tokio::test]
async fn a_missing_signature_header_is_rejected() {
    let config = WebhookConfig::new("integration-secret", "ci");
    let handler = Arc::new(RecordingHandler {
        calls: AtomicUsize::new(0),
        last_payload: std::sync::Mutex::new(None),
    });
    let state = Arc::new(InboundState {
        config,
        handler: handler.clone(),
    });

    let request = Request::builder()
        .method("POST")
        .uri("/webhook")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let response = router(state).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_stale_timestamp_is_rejected_when_replay_protection_is_enabled() {
    let config = WebhookConfig::new("integration-secret", "ci")
        .with_replay_protection("x-webhook-timestamp")
        .with_replay_window_secs(60);
    let handler = Arc::new(RecordingHandler {
        calls: AtomicUsize::new(0),
        last_payload: std::sync::Mutex::new(None),
    });
    let state = Arc::new(InboundState {
        config: config.clone(),
        handler: handler.clone(),
    });

    let body = b"{}";
    // Ten minutes old — well outside the 60s window.
    let stale_ts = (chrono::Utc::now().timestamp() - 600).to_string();
    let signature = {
        let mut mac = <hmac::Hmac<sha2::Sha256> as hmac::digest::KeyInit>::new_from_slice(
            b"integration-secret",
        )
        .unwrap();
        use hmac::Mac;
        mac.update(stale_ts.as_bytes());
        mac.update(b".");
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    };

    let request = Request::builder()
        .method("POST")
        .uri("/webhook")
        .header("x-webhook-signature", signature)
        .header("x-webhook-timestamp", stale_ts)
        .header("content-type", "application/json")
        .body(Body::from(body.as_slice()))
        .unwrap();

    let response = router(state).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 0);
    let _ = config; // config kept alive for clarity of what was signed against
}

#[tokio::test]
async fn a_non_json_body_still_reaches_the_handler_as_a_string_payload() {
    let config = WebhookConfig::new("integration-secret", "ci");
    let handler = Arc::new(RecordingHandler {
        calls: AtomicUsize::new(0),
        last_payload: std::sync::Mutex::new(None),
    });
    let state = Arc::new(InboundState {
        config: config.clone(),
        handler: handler.clone(),
    });

    let body = b"not json at all";
    let signature = ardur_webhook::sign_body(body, &config.secret).unwrap();

    let request = Request::builder()
        .method("POST")
        .uri("/webhook")
        .header("x-webhook-signature", signature)
        .body(Body::from(body.as_slice()))
        .unwrap();

    let response = router(state).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *handler.last_payload.lock().unwrap(),
        Some(serde_json::Value::String("not json at all".to_string()))
    );
}
