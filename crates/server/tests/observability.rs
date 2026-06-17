//! Observability HTTP surface tests for `/health`, `/metrics`, and the
//! bearer-gated admin runtime inspection API.

mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode};

const ADMIN_TOKEN: &str = "server-admin-token-000000000000";

#[tokio::test]
async fn health_returns_ok_with_dependency_checks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config);

    let request = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .expect("request builds");

    let (status, body) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);

    let json: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");
    assert_eq!(json["status"], "ok");
    assert_eq!(json["dependencies"]["data_dir"], "ok");
    assert_eq!(json["dependencies"]["journal"], "ok");
    assert_eq!(json["dependencies"]["worker"], "ok");
}

#[tokio::test]
async fn metrics_are_prometheus_parseable_and_do_not_leak_tokens() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config);

    let request = Request::builder()
        .method("GET")
        .uri("/metrics")
        .body(Body::empty())
        .expect("request builds");

    let (status, body) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);

    let metrics = std::str::from_utf8(&body).expect("metrics are utf8");
    assert!(metrics.contains("# HELP ardur_server_build_info"));
    assert!(metrics.contains("# TYPE ardur_server_build_info gauge"));
    assert!(metrics.contains("ardur_server_build_info{version="));
    assert!(metrics.contains("ardur_server_receipts_total"));
    assert!(metrics.contains("ardur_server_admin_bearer_tokens_configured"));

    for secret in [
        support::BOT_TOKEN,
        support::SIGNING_SECRET,
        support::CHAT_TOKEN,
        ADMIN_TOKEN,
    ] {
        assert!(
            !metrics.contains(secret),
            "metrics must not leak configured secret/token value {secret:?}: {metrics}"
        );
    }
}

#[tokio::test]
async fn admin_runtime_requires_auth_and_returns_redacted_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config);

    let missing = Request::builder()
        .method("GET")
        .uri("/admin/runtime")
        .body(Body::empty())
        .expect("request builds");
    let (missing_status, _) = support::oneshot(router.clone(), missing).await;
    assert_eq!(missing_status, StatusCode::UNAUTHORIZED);

    let invalid = Request::builder()
        .method("GET")
        .uri("/admin/runtime")
        .header("Authorization", "Bearer wrong-token")
        .body(Body::empty())
        .expect("request builds");
    let (invalid_status, _) = support::oneshot(router.clone(), invalid).await;
    assert_eq!(invalid_status, StatusCode::UNAUTHORIZED);

    let valid = Request::builder()
        .method("GET")
        .uri("/admin/runtime")
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");
    let (status, body) = support::oneshot(router, valid).await;
    assert_eq!(status, StatusCode::OK);

    let json: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");
    assert_eq!(json["cap_tokens"]["audience"], "ardur");
    assert_eq!(json["cap_tokens"]["gateway_subject"], "ardur:slack-gateway");
    assert_eq!(json["gates"]["cost_budget_cents"], 10_000);
    assert!(json["receipts"]["count"].is_u64());
    assert!(
        json["tools"]["allowlist_count"]
            .as_u64()
            .unwrap_or_default()
            >= 1
    );

    let body_text = std::str::from_utf8(&body).expect("admin body is utf8");
    for secret in [
        support::BOT_TOKEN,
        support::SIGNING_SECRET,
        support::CHAT_TOKEN,
        ADMIN_TOKEN,
    ] {
        assert!(
            !body_text.contains(secret),
            "admin snapshot must not leak configured secret/token value {secret:?}: {body_text}"
        );
    }
}

#[tokio::test]
async fn admin_runtime_fails_closed_when_no_admin_tokens_configured() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config_with_admin(&dir, None, Vec::new());
    let router = support::boot_router(&config);

    let request = Request::builder()
        .method("GET")
        .uri("/admin/runtime")
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");

    let (status, body) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");
    assert_eq!(json["error"], "missing or invalid bearer token");
}
