//! Fail-closed CORS for the PWA origin allowlist.
//!
//! Default boots emit no `Access-Control-Allow-Origin`. An explicit
//! `ARDUR_CORS_ORIGINS` entry reflects only that exact Origin on `/chat` and
//! `/approvals*`. Wildcard origins are refused at config load.

mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::json;
use tower::ServiceExt as _;

const PWA_ORIGIN: &str = "http://127.0.0.1:4173";

fn cors_config(dir: &tempfile::TempDir, origins: Vec<String>) -> ardur_server::Config {
    ardur_server::Config {
        cors_origins: origins,
        ..support::test_config(dir, None)
    }
}

#[tokio::test]
async fn default_boot_emits_no_cors_headers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let router = support::boot_router(&support::test_config(&dir, None)).await;

    let request = Request::builder()
        .method("POST")
        .uri("/chat")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ORIGIN, PWA_ORIGIN)
        .header("authorization", format!("Bearer {}", support::CHAT_TOKEN))
        .body(Body::from(
            json!({ "message": "hello", "stream": true }).to_string(),
        ))
        .expect("request builds");
    let response = router.oneshot(request).await.expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none(),
        "empty cors_origins must not reflect Origin"
    );
}

#[tokio::test]
async fn allowlisted_origin_is_reflected_on_chat() {
    let dir = tempfile::tempdir().expect("tempdir");
    let router = support::boot_router(&cors_config(&dir, vec![PWA_ORIGIN.to_string()])).await;

    let request = Request::builder()
        .method("POST")
        .uri("/chat")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ORIGIN, PWA_ORIGIN)
        .header("authorization", format!("Bearer {}", support::CHAT_TOKEN))
        .body(Body::from(
            json!({ "message": "hello", "stream": true }).to_string(),
        ))
        .expect("request builds");
    let response = router.oneshot(request).await.expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);
    let allowed = response
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        .expect("allowlisted origin is reflected")
        .to_str()
        .expect("ascii origin");
    assert_eq!(allowed, PWA_ORIGIN);
    assert_eq!(
        response
            .headers()
            .get(header::VARY)
            .map(|v| v.to_str().unwrap_or_default()),
        Some("Origin")
    );
}

#[tokio::test]
async fn unknown_origin_is_not_reflected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let router = support::boot_router(&cors_config(&dir, vec![PWA_ORIGIN.to_string()])).await;

    let request = Request::builder()
        .method("POST")
        .uri("/chat")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ORIGIN, "http://evil.example")
        .header("authorization", format!("Bearer {}", support::CHAT_TOKEN))
        .body(Body::from(
            json!({ "message": "hello", "stream": true }).to_string(),
        ))
        .expect("request builds");
    let response = router.oneshot(request).await.expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none(),
        "unknown origins must not be reflected"
    );
}

#[tokio::test]
async fn preflight_options_chat_reflects_allowlisted_origin() {
    let dir = tempfile::tempdir().expect("tempdir");
    let router = support::boot_router(&cors_config(&dir, vec![PWA_ORIGIN.to_string()])).await;

    let request = Request::builder()
        .method("OPTIONS")
        .uri("/chat")
        .header(header::ORIGIN, PWA_ORIGIN)
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .header(
            header::ACCESS_CONTROL_REQUEST_HEADERS,
            "authorization,content-type",
        )
        .body(Body::empty())
        .expect("request builds");
    let response = router.oneshot(request).await.expect("router responds");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok()),
        Some(PWA_ORIGIN)
    );
    let allow_headers = response
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(allow_headers.contains("authorization"));
    assert!(allow_headers.contains("content-type"));
}

#[tokio::test]
async fn preflight_without_allowlist_does_not_reflect_origin() {
    let dir = tempfile::tempdir().expect("tempdir");
    let router = support::boot_router(&support::test_config(&dir, None)).await;

    let request = Request::builder()
        .method("OPTIONS")
        .uri("/chat")
        .header(header::ORIGIN, PWA_ORIGIN)
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .body(Body::empty())
        .expect("request builds");
    let response = router.oneshot(request).await.expect("router responds");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none()
    );
}
