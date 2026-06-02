//! `slack_url_verification` — a signed `url_verification` handshake is verified
//! and echoes its `challenge` back in the 200 response body.

mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode};

#[tokio::test]
async fn signed_url_verification_echoes_challenge() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, None);
    let router = support::boot_router(&config);

    let ts = support::now_unix_string();
    let body = serde_json::json!({
        "type": "url_verification",
        "challenge": "challenge-token-xyz",
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

    let (status, resp_body) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);

    let json: serde_json::Value = serde_json::from_slice(&resp_body).expect("body is JSON");
    assert_eq!(json["challenge"], "challenge-token-xyz");
}
