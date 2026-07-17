//! HTTP surface tests for the approval-gate *decide* endpoints: `GET /approvals`,
//! `POST /approvals/{id}/approve`, and `POST /approvals/{id}/reject`.
//!
//! These endpoints are admin-bearer gated (fail closed with no admin tokens) and
//! back onto the same on-disk store the CLI uses (`<data_dir>/approvals/<id>.json`).

mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode};

const ADMIN_TOKEN: &str = "server-admin-token-000000000000";

/// Seed a pending approval card at `<data_dir>/approvals/<id>.json` and return
/// the file path.
fn seed_pending(data_dir: &std::path::Path, id: &str) -> std::path::PathBuf {
    let approvals = data_dir.join("approvals");
    std::fs::create_dir_all(&approvals).expect("approvals dir");
    let path = approvals.join(format!("{id}.json"));
    let card = serde_json::json!({
        "id": id,
        "status": "pending",
        "summary": "delete the production database",
    });
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&card).expect("card serializes"),
    )
    .expect("seed pending card");
    path
}

#[tokio::test]
async fn approve_requires_auth_and_flips_status() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-approve-001";
    let card_path = seed_pending(dir.path(), id);

    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config).await;

    // Missing token → 401, and the card is untouched.
    let missing = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .body(Body::empty())
        .expect("request builds");
    let (missing_status, _) = support::oneshot(router.clone(), missing).await;
    assert_eq!(missing_status, StatusCode::UNAUTHORIZED);

    // Wrong token → 401.
    let invalid = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .header("Authorization", "Bearer wrong-token")
        .body(Body::empty())
        .expect("request builds");
    let (invalid_status, _) = support::oneshot(router.clone(), invalid).await;
    assert_eq!(invalid_status, StatusCode::UNAUTHORIZED);

    // Still pending on disk after the rejected attempts.
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&card_path).expect("read card"))
            .expect("card parses");
    assert_eq!(on_disk["status"], "pending");

    // Valid token → 200 and the response echoes the flipped record.
    let valid = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");
    let (status, body) = support::oneshot(router.clone(), valid).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");
    assert_eq!(json["status"], "approved");
    assert!(json["decided_at"].is_u64());

    // Persisted to the shared on-disk store.
    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&card_path).expect("read card"))
            .expect("card parses");
    assert_eq!(on_disk["status"], "approved");
    assert!(on_disk["decided_at"].is_u64());

    // Idempotent-safe: a second approve on an already-decided card → 409.
    let again = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");
    let (again_status, _) = support::oneshot(router, again).await;
    assert_eq!(again_status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn reject_stores_denied_status_and_reason() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-reject-001";
    let card_path = seed_pending(dir.path(), id);

    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config).await;

    // The PWA wire verb is `reject`; the stored status must be `denied`.
    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/reject"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"reason":"too risky"}"#))
        .expect("request builds");
    let (status, body) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");
    assert_eq!(json["status"], "denied");
    assert_eq!(json["deny_reason"], "too risky");

    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&card_path).expect("read card"))
            .expect("card parses");
    assert_eq!(on_disk["status"], "denied");
    assert_eq!(on_disk["deny_reason"], "too risky");
}

#[tokio::test]
async fn reject_without_body_defaults_empty_reason() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-reject-nobody";
    seed_pending(dir.path(), id);

    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config).await;

    // The PWA sends no body at all — this must still succeed.
    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/reject"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");
    let (status, body) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");
    assert_eq!(json["status"], "denied");
    assert_eq!(json["deny_reason"], "");
}

#[tokio::test]
async fn missing_card_returns_404_and_malformed_id_returns_400() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config).await;

    // Well-formed but nonexistent id → 404.
    let missing = Request::builder()
        .method("POST")
        .uri("/approvals/does-not-exist/approve")
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");
    let (missing_status, _) = support::oneshot(router.clone(), missing).await;
    assert_eq!(missing_status, StatusCode::NOT_FOUND);

    // A traversal-shaped id (percent-encoded dots/slash) must never be joined onto
    // a path — it is refused as malformed (400) rather than escaping the dir.
    let traversal = Request::builder()
        .method("POST")
        .uri("/approvals/..%2f..%2fetc%2fpasswd/approve")
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");
    let (traversal_status, _) = support::oneshot(router, traversal).await;
    assert!(
        traversal_status == StatusCode::BAD_REQUEST || traversal_status == StatusCode::NOT_FOUND,
        "traversal id must be refused, got {traversal_status}"
    );
}

#[tokio::test]
async fn list_returns_cards_and_fails_closed_without_admin_tokens() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed_pending(dir.path(), "card-list-a");
    seed_pending(dir.path(), "card-list-b");

    // Fail-closed: no admin tokens configured → 401 even with a plausible token.
    let closed_config = support::test_config_with_admin(&dir, None, Vec::new());
    let closed_router = support::boot_router(&closed_config).await;
    let closed = Request::builder()
        .method("GET")
        .uri("/approvals")
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");
    let (closed_status, _) = support::oneshot(closed_router, closed).await;
    assert_eq!(closed_status, StatusCode::UNAUTHORIZED);

    // With admin tokens, list returns both seeded cards, each carrying its id.
    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config).await;
    let request = Request::builder()
        .method("GET")
        .uri("/approvals")
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");
    let (status, body) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");
    let cards = json.as_array().expect("list is a JSON array");
    assert_eq!(cards.len(), 2);
    let ids: Vec<&str> = cards.iter().filter_map(|c| c["id"].as_str()).collect();
    assert!(ids.contains(&"card-list-a"));
    assert!(ids.contains(&"card-list-b"));
}
