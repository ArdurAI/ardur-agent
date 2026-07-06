//! `healthz` — `GET /healthz` returns 200 with the build-metadata JSON, driven
//! through the real router in-process (no socket).

mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode};

#[tokio::test]
async fn healthz_returns_ok_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config(&dir, None);
    let router = support::boot_router(&config);

    let request = Request::builder()
        .method("GET")
        .uri("/healthz")
        .body(Body::empty())
        .expect("request builds");

    let (status, body) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);

    let json: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");
    assert_eq!(json["status"], "ok");
    assert!(json["build"].is_string(), "carries a build version");
}
