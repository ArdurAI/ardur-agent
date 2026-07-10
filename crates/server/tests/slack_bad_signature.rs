//! `slack_bad_signature` — a request whose `v0=` signature does not match the
//! signing secret is rejected with 401 (fail closed), before any processing.

mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode};

#[tokio::test]
async fn bad_signature_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, None);
    let router = support::boot_router(&config).await;

    let ts = support::now_unix_string();
    let body = serde_json::json!({
        "type": "url_verification",
        "challenge": "should-not-be-echoed",
    })
    .to_string();

    // A plausible-looking but wrong signature (not computed from the secret).
    let forged = "v0=0000000000000000000000000000000000000000000000000000000000000000";

    let request = Request::builder()
        .method("POST")
        .uri("/slack/events")
        .header("X-Slack-Signature", forged)
        .header("X-Slack-Request-Timestamp", ts)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("request builds");

    let (status, _body) = support::oneshot(router, request).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a forged signature must fail closed"
    );
}
