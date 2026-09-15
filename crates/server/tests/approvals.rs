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

/// **ARD-139.** `POST /approvals/{id}/approve` mints a real signed
/// `approval.approve.accepted.v1` receipt, chained onto the same
/// `<data_dir>/receipts/chain.jsonl` a chat turn over the same data dir
/// would append to, and echoes the minted `receipt_id` in the response.
#[tokio::test]
async fn approve_mints_a_signed_receipt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-receipt-001";
    seed_pending(dir.path(), id);

    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config).await;

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");
    let (status, body) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");
    let receipt_id = json["receipt_id"]
        .as_str()
        .expect("the response echoes a minted receipt_id");

    let chain =
        ardur_fused_runtime::load_persisted_chain(dir.path().join("receipts").join("chain.jsonl"))
            .expect("chain loads");
    assert_eq!(
        chain.len(),
        1,
        "exactly one receipt minted for this decision"
    );
    assert_eq!(chain[0].body.verb.as_str(), "approval.approve.accepted.v1");
    assert_eq!(chain[0].body.receipt_id.to_string(), receipt_id);
    ardur_fused_runtime::verify_persisted_chain(&chain).expect("the chain verifies");
}

/// A reject also mints its own receipt verb.
#[tokio::test]
async fn reject_mints_a_signed_receipt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-receipt-002";
    seed_pending(dir.path(), id);

    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config).await;

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/reject"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");
    let (status, _body) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);

    let chain =
        ardur_fused_runtime::load_persisted_chain(dir.path().join("receipts").join("chain.jsonl"))
            .expect("chain loads");
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].body.verb.as_str(), "approval.reject.accepted.v1");
}

/// gh#497: two concurrent HTTP decisions over the same pending card must
/// produce exactly one 200 and one explicit 409 conflict, with the durable
/// record matching the winner.
///
/// RED on the reviewed baseline: the route's read/check/rename sequence lets
/// both requests observe `pending` and both persist, so both return 200 and
/// the last rename silently wins. Run on a multi-thread runtime so the two
/// handlers execute with true parallelism — a current-thread runtime cannot
/// interleave their synchronous file I/O and would hide the race.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_http_decisions_have_exactly_one_winner() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-race-001";
    let card_path = seed_pending(dir.path(), id);

    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config).await;

    let approve = {
        let router = router.clone();
        tokio::spawn(async move {
            let request = Request::builder()
                .method("POST")
                .uri(format!("/approvals/{id}/approve"))
                .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
                .body(Body::empty())
                .expect("request builds");
            support::oneshot(router, request).await.0
        })
    };
    let reject = {
        let router = router.clone();
        tokio::spawn(async move {
            let request = Request::builder()
                .method("POST")
                .uri(format!("/approvals/{id}/reject"))
                .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
                .body(Body::empty())
                .expect("request builds");
            support::oneshot(router, request).await.0
        })
    };
    let (approve_status, reject_status) = (
        approve.await.expect("approve task"),
        reject.await.expect("reject task"),
    );

    let winners =
        (approve_status == StatusCode::OK) as u32 + (reject_status == StatusCode::OK) as u32;
    assert_eq!(
        winners, 1,
        "exactly one HTTP decision may succeed: approve={approve_status} reject={reject_status}"
    );
    let loser_status = if approve_status == StatusCode::OK {
        reject_status
    } else {
        approve_status
    };
    assert_eq!(
        loser_status,
        StatusCode::CONFLICT,
        "the losing decision must be an explicit conflict"
    );

    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&card_path).expect("read card"))
            .expect("card parses");
    let expected = if approve_status == StatusCode::OK {
        "approved"
    } else {
        "denied"
    };
    assert_eq!(
        on_disk["status"], expected,
        "the durable record must match the sole winner"
    );
}

/// gh#417/E4.1: a decision whose receipt mint fails after the card is durable
/// must leave an OBSERVABLE, DURABLE pending audit obligation — in the 200
/// response and on the card itself — never a silent log line.
///
/// RED on the reviewed baseline: the mint failure is only `tracing::error!`d,
/// so neither field exists.
#[tokio::test]
async fn a_failed_decision_receipt_mint_leaves_an_observable_audit_obligation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-audit-pending-001";
    let card_path = seed_pending(dir.path(), id);

    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let state = support::boot_stub(&config).await;
    let router = ardur_server::build_router(state.clone());
    // Shut the worker down: the decision write is plain file I/O and still
    // succeeds, but the receipt mint has no worker to run on and must fail.
    state.shutdown();

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");
    let (status, body) = support::oneshot(router, request).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "the durable decision itself still succeeds; only its audit is pending"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");
    assert_eq!(json["status"], "approved");
    assert!(
        json.get("receipt_id").is_none(),
        "no receipt was minted, so none may be reported: {json}"
    );
    assert_eq!(
        json["audit_pending"], true,
        "the response must surface the unmet audit obligation: {json}"
    );

    let on_disk: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&card_path).expect("read card"))
            .expect("card parses");
    assert_eq!(on_disk["status"], "approved");
    assert_eq!(
        on_disk["audit_pending"], true,
        "the obligation must be durable on the card, not log-only: {on_disk}"
    );
}

/// Review P2: deciding a legacy card over HTTP preserves its original shape —
/// the 200 body and the on-disk record gain the decision/audit fields only,
/// never phantom `tool`/`capability`/`arguments_digest`/`reason`/`created_at`.
#[tokio::test]
async fn a_decision_preserves_a_legacy_cards_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-legacy-shape";
    let card_path = seed_pending(dir.path(), id);

    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config).await;

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");
    let (status, body) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);

    for (label, raw) in [
        (
            "response",
            serde_json::from_slice::<serde_json::Value>(&body).expect("body is JSON"),
        ),
        (
            "on-disk",
            serde_json::from_str::<serde_json::Value>(
                &std::fs::read_to_string(&card_path).expect("read card"),
            )
            .expect("card parses"),
        ),
    ] {
        let object = raw.as_object().expect("card is an object");
        for absent in [
            "tool",
            "capability",
            "arguments_digest",
            "reason",
            "created_at",
        ] {
            assert!(
                !object.contains_key(absent),
                "{label}: deciding must not invent a `{absent}` field on a legacy card: {raw}"
            );
        }
    }

    let json: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");
    assert_eq!(json["status"], "approved");
    assert_eq!(json["summary"], "delete the production database");
    assert!(
        json["receipt_id"].is_string(),
        "a settled decision links its receipt: {json}"
    );
    assert!(
        json.get("audit_pending").is_none(),
        "a fully-audited decision carries no obligation: {json}"
    );
}

/// Review P2: the 500 bodies keep their legacy distinctions — read failure,
/// misshapen (valid JSON, not an object), corrupt (unparseable), and write
/// failure report four different messages.
#[tokio::test]
async fn error_bodies_distinguish_read_shape_and_write_failures() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let approvals = dir.path().join("approvals");
    std::fs::create_dir_all(&approvals).expect("approvals dir");

    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config).await;

    async fn decide(router: axum::Router, id: &str) -> (StatusCode, axum::body::Bytes) {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/approvals/{id}/approve"))
            .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
            .body(Body::empty())
            .expect("request builds");
        support::oneshot(router, request).await
    }

    // Corrupt (unparseable JSON).
    std::fs::write(approvals.join("card-corrupt.json"), "{not json").expect("seed corrupt");
    let (status, body) = decide(router.clone(), "card-corrupt").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");
    assert_eq!(json["error"], "approval record is corrupt");

    // Misshapen (valid JSON, not an object).
    std::fs::write(approvals.join("card-array.json"), "[1, 2, 3]").expect("seed misshapen");
    let (status, body) = decide(router.clone(), "card-array").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");
    assert_eq!(json["error"], "approval record is not an object");

    // Read failure (unreadable card file).
    let unreadable = approvals.join("card-unreadable.json");
    std::fs::write(
        &unreadable,
        serde_json::json!({ "id": "card-unreadable", "status": "pending" }).to_string(),
    )
    .expect("seed unreadable");
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000))
        .expect("chmod the card unreadable");
    let (status, body) = decide(router.clone(), "card-unreadable").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");
    assert_eq!(json["error"], "failed to read approval record");
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o644))
        .expect("restore the card");

    // Write failure (read-only store directory: the lock file cannot be
    // created, so the transaction cannot start).
    let pending = approvals.join("card-pending.json");
    std::fs::write(
        &pending,
        serde_json::json!({ "id": "card-pending", "status": "pending" }).to_string(),
    )
    .expect("seed pending");
    let mut perms = std::fs::metadata(&approvals)
        .expect("dir metadata")
        .permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(&approvals, perms.clone()).expect("chmod the dir read-only");
    let (status, body) = decide(router.clone(), "card-pending").await;
    perms.set_mode(0o755);
    std::fs::set_permissions(&approvals, perms).expect("restore the dir");
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let json: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");
    assert_eq!(json["error"], "failed to persist approval decision");
}
