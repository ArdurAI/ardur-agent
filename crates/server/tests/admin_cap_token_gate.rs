//! Cap-token gating for admin mutation endpoints (gh#417).
//!
//! gh#417 requires a write-admin surface that is "cap-token gated end to end
//! (every mutation = receipt)". The receipt half already exists:
//! `apply_approval_decision` persists atomically, appends a journal audit, and
//! mints a signed receipt. The gate half does not — `authorize_admin` is
//! bearer-only, so no route verifies a cap-token at all.
//!
//! A bearer token is one shared secret with no audience, no expiry, no
//! per-verb allowlist and no revocation; presenting it grants every admin
//! route at once. A cap-token carries those constraints, so authority to
//! decide an approval can be handed out without also handing out authority to
//! rewrite config.
//!
//! The gate is ADDITIVE, not a replacement: the bearer check still answers
//! "who are you" with 401, and the cap-token answers "may you do THIS" with
//! 403. So every request below carries BOTH, as a real admin client would.
//! Accepting a cap-token *instead of* the bearer would let the new gate widen
//! access rather than narrow it.
//!
//! These tests pin the gate on the existing approvals mutation rather than
//! inventing a second mutation pattern.

mod support;

use ardur_cap_token::{
    BiscuitCapTokenIssuer, CapScope, CapTokenIssuer, HolderId as CapHolderId, KeyPair,
};
// `ardur-cap-token` re-exports KeyPair/PublicKey but not PrivateKey, and
// loading the persisted issuer key needs `PrivateKey::from_bytes_hex` — the
// same reason `crates/server` and `crates/e2e-tests` pull biscuit-auth direct.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use biscuit_auth::{Algorithm, PrivateKey};

const ADMIN_TOKEN: &str = "server-admin-token-000000000000";
// Must match `ardur_server::state::AUDIENCE`; a token minted for another
// audience is refused by the verifier regardless of its verb list.
const AUDIENCE: &str = "ardur";
const HOLDER: &str = "admin-operator";

/// The verb a cap-token must name to decide an approval.
// Matches both the route gate AND `APPROVAL_DECIDE_TOOL` in state.rs, so a
// token that passes the gate can also mint the decision receipt.
const APPROVAL_DECIDE_VERB: &str = "approval.decide";

/// Well past any plausible test clock, so expiry is never the reason a token
/// is rejected except in the test that asks for it.
fn far_future() -> u64 {
    u64::from(u32::MAX)
}

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

fn status_on_disk(card_path: &std::path::Path) -> String {
    let raw = std::fs::read_to_string(card_path).expect("card readable");
    let card: serde_json::Value = serde_json::from_str(&raw).expect("card parses");
    card["status"]
        .as_str()
        .expect("status is a string")
        .to_string()
}

/// The issuer key the SERVER will verify against.
///
/// `AppState` mints and persists this at `<data_dir>/keys/issuer.key` on boot,
/// so the test reads it back rather than requiring a new public export. Call
/// after `boot_router`, which is what creates the file.
fn server_issuer_keypair(data_dir: &std::path::Path) -> KeyPair {
    let hex = std::fs::read_to_string(data_dir.join("keys").join("issuer.key"))
        .expect("the server persisted an issuer key on boot");
    let private = PrivateKey::from_bytes_hex(hex.trim(), Algorithm::Ed25519)
        .expect("the persisted issuer key parses");
    KeyPair::from(&private)
}

/// Mint a serialized cap-token naming `verbs`, signed by `keypair`.
fn mint(keypair: KeyPair, verbs: &[&str], expires_unix: u64) -> String {
    BiscuitCapTokenIssuer::new(keypair)
        .issue(
            CapHolderId(HOLDER.to_string()),
            CapScope {
                audience: AUDIENCE.to_string(),
                expires_unix,
                budget_remaining: u64::MAX,
                tool_allowlist: verbs.iter().map(|v| (*v).to_string()).collect(),
            },
        )
        .expect("a token is issued")
        .to_base64()
        .expect("the token serializes")
}

/// A cap-token naming the verb is accepted and the mutation happens.
#[tokio::test]
async fn a_cap_token_naming_the_verb_is_accepted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-cap-accept";
    let card_path = seed_pending(dir.path(), id);

    let mut config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    config.admin_cap_token_gate = true;
    let router = support::boot_router(&config).await;
    let keypair = server_issuer_keypair(&config.data_dir);

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header(
            "X-Ardur-Cap-Token",
            mint(keypair, &[APPROVAL_DECIDE_VERB], far_future()),
        )
        .body(Body::empty())
        .expect("request builds");
    let (status, _) = support::oneshot(router, request).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "a token naming the verb is accepted"
    );
    assert_eq!(status_on_disk(&card_path), "approved");
}

/// A cap-token that does not name the verb is refused, and nothing mutates.
///
/// The contrast case, and the point of the slice: a gate that accepted any
/// well-formed token would pass the test above while behaving exactly like the
/// shared secret it replaces. The on-disk assertion matters because gating
/// that rejects the *response* after performing the write is theatre.
#[tokio::test]
async fn a_cap_token_without_the_verb_is_refused_and_nothing_mutates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-cap-wrong-verb";
    let card_path = seed_pending(dir.path(), id);

    let mut config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    config.admin_cap_token_gate = true;
    let router = support::boot_router(&config).await;
    let keypair = server_issuer_keypair(&config.data_dir);

    // A legitimately-issued token for a DIFFERENT admin verb.
    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header(
            "X-Ardur-Cap-Token",
            mint(keypair, &["admin.config.write"], far_future()),
        )
        .body(Body::empty())
        .expect("request builds");
    let (status, _) = support::oneshot(router, request).await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "authority for one admin verb must not grant another"
    );
    assert_eq!(
        status_on_disk(&card_path),
        "pending",
        "a denied request must not have decided the card"
    );
}

/// An expired token is refused even though it names the verb.
#[tokio::test]
async fn an_expired_cap_token_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-cap-expired";
    seed_pending(dir.path(), id);

    let mut config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    config.admin_cap_token_gate = true;
    let router = support::boot_router(&config).await;
    let keypair = server_issuer_keypair(&config.data_dir);

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header(
            "X-Ardur-Cap-Token",
            mint(keypair, &[APPROVAL_DECIDE_VERB], 1),
        )
        .body(Body::empty())
        .expect("request builds");
    let (status, _) = support::oneshot(router, request).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// A token signed by an unknown key is refused.
///
/// Without this, a gate that parsed the token and read its allowlist without
/// checking the signature would pass every test above — and any caller could
/// mint their own authority.
#[tokio::test]
async fn a_token_signed_by_an_unknown_key_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-cap-forged";
    let card_path = seed_pending(dir.path(), id);

    let mut config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    config.admin_cap_token_gate = true;
    let router = support::boot_router(&config).await;

    let attacker = KeyPair::new();
    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header(
            "X-Ardur-Cap-Token",
            mint(attacker, &[APPROVAL_DECIDE_VERB], far_future()),
        )
        .body(Body::empty())
        .expect("request builds");
    let (status, _) = support::oneshot(router, request).await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a self-signed token must not authorize anything"
    );
    assert_eq!(status_on_disk(&card_path), "pending");
}

/// A garbage header value is refused rather than panicked on.
#[tokio::test]
async fn a_malformed_cap_token_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-cap-garbage";
    seed_pending(dir.path(), id);

    let mut config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    config.admin_cap_token_gate = true;
    let router = support::boot_router(&config).await;

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header("X-Ardur-Cap-Token", "not-a-biscuit")
        .body(Body::empty())
        .expect("request builds");
    let (status, _) = support::oneshot(router, request).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// With the gate on, a bare bearer token no longer suffices for a mutation.
///
/// If the weaker credential still passed, the stronger gate would be
/// decorative — the shared secret would remain the real boundary.
#[tokio::test]
async fn a_bearer_token_alone_does_not_pass_the_cap_gate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-cap-bearer-only";
    let card_path = seed_pending(dir.path(), id);

    let mut config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    config.admin_cap_token_gate = true;
    let router = support::boot_router(&config).await;

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");
    let (status, _) = support::oneshot(router, request).await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "with the gate on, the shared secret is not enough"
    );
    assert_eq!(status_on_disk(&card_path), "pending");
}

/// The gate is opt-in: un-opted deployments keep the bearer path exactly.
///
/// A security control that breaks every current operator on upgrade gets
/// switched off in a hurry.
#[tokio::test]
async fn without_the_gate_configured_bearer_still_works() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-cap-off";
    let card_path = seed_pending(dir.path(), id);

    // No `admin_cap_token_gate` — the default.
    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config).await;

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");
    let (status, _) = support::oneshot(router, request).await;

    assert_eq!(
        status,
        StatusCode::OK,
        "the gate is opt-in; un-opted deployments are unaffected"
    );
    assert_eq!(status_on_disk(&card_path), "approved");
}

/// The decision receipt records the capability that actually authorized it.
///
/// Review P1, and it was a real hole: the receipt worker minted a FRESH
/// gateway-subject token, so every gated decision was attributed to
/// `ardur:slack-gateway` regardless of which delegated token the operator
/// presented. An audit chain that cannot say which capability performed an
/// action is not doing the job the chain exists for.
#[tokio::test]
async fn the_receipt_binds_to_the_presented_capability() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-receipt-binding";
    seed_pending(dir.path(), id);

    let mut config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    config.admin_cap_token_gate = true;
    let router = support::boot_router(&config).await;
    let keypair = server_issuer_keypair(&config.data_dir);

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header(
            "X-Ardur-Cap-Token",
            mint(keypair, &[APPROVAL_DECIDE_VERB], far_future()),
        )
        .body(Body::empty())
        .expect("request builds");
    let (status, _) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);

    let chain =
        ardur_fused_runtime::load_persisted_chain(dir.path().join("receipts").join("chain.jsonl"))
            .expect("chain loads");
    assert_eq!(chain.len(), 1, "exactly one receipt for this decision");
    assert_eq!(
        chain[0].body.subject.0, HOLDER,
        "the receipt must name the operator who presented the token, not the \
         gateway that happened to mint one"
    );
    ardur_fused_runtime::verify_persisted_chain(&chain).expect("the chain verifies");
}

/// Without the gate, the receipt still mints under the gateway subject.
///
/// The contrast case: un-gated deployments are unchanged, so the binding above
/// is demonstrably caused by the presented token rather than by anything else
/// in the decision path.
#[tokio::test]
async fn without_the_gate_the_receipt_keeps_the_gateway_subject() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-receipt-ungated";
    seed_pending(dir.path(), id);

    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config).await;

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");
    let (status, _) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);

    let chain =
        ardur_fused_runtime::load_persisted_chain(dir.path().join("receipts").join("chain.jsonl"))
            .expect("chain loads");
    assert_ne!(
        chain[0].body.subject.0, HOLDER,
        "an un-gated decision has no presented token to bind to"
    );
}

/// The CORS preflight advertises the cap-token header.
///
/// Review P2: the header is non-safelisted, so a cross-origin client preflights
/// before sending it. Omitting it from `Access-Control-Allow-Headers` would
/// make the gate unusable from the PWA even with a perfectly valid token —
/// the browser blocks the request before the handler ever runs.
#[tokio::test]
async fn the_preflight_allows_the_cap_token_header() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    // An empty allowlist reflects no Origin at all, so the preflight would
    // carry no CORS headers and the assertion below would be vacuous.
    config.cors_origins = vec!["http://localhost:5173".to_string()];
    let router = support::boot_router(&config).await;

    let request = Request::builder()
        .method("OPTIONS")
        .uri("/approvals/card-any/approve")
        .header("Origin", "http://localhost:5173")
        .body(Body::empty())
        .expect("request builds");
    let response = tower::ServiceExt::oneshot(router, request)
        .await
        .expect("the router responds");

    let allowed = response
        .headers()
        .get("Access-Control-Allow-Headers")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        allowed.contains("X-Ardur-Cap-Token"),
        "the preflight must advertise the cap-token header, got `{allowed}`"
    );
}

/// The OpenAPI spec documents the cap-token requirement when the gate is on.
///
/// Review P2: a client generated from a spec that describes these operations
/// as bearer-only would never send the credential, and would fail against
/// every gated deployment with no indication why.
#[tokio::test]
async fn the_openapi_spec_documents_the_gate_when_enabled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    config.admin_cap_token_gate = true;
    let router = support::boot_router(&config).await;

    let request = Request::builder()
        .uri("/openapi.json")
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");
    let (status, body) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);
    let spec: serde_json::Value = serde_json::from_slice(&body).expect("spec is JSON");

    for op in ["/approvals/{id}/approve", "/approvals/{id}/reject"] {
        let post = &spec["paths"][op]["post"];
        let params = post["parameters"].as_array().expect("parameters");
        assert!(
            params
                .iter()
                .any(|p| p["name"] == "X-Ardur-Cap-Token" && p["in"] == "header"),
            "{op} must declare the cap-token header: {post}"
        );
        assert!(
            post["responses"]["403"].is_object(),
            "{op} must document the forbidden response"
        );
    }
}

/// Without the gate, the spec is unchanged.
///
/// The contrast case: a client generated against an un-gated deployment must
/// not be told to send a credential that deployment ignores.
#[tokio::test]
async fn the_openapi_spec_is_unchanged_without_the_gate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config).await;

    let request = Request::builder()
        .uri("/openapi.json")
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .body(Body::empty())
        .expect("request builds");
    let (_, body) = support::oneshot(router, request).await;
    let spec: serde_json::Value = serde_json::from_slice(&body).expect("spec is JSON");

    let post = &spec["paths"]["/approvals/{id}/approve"]["post"];
    assert!(
        !post["parameters"]
            .as_array()
            .expect("parameters")
            .iter()
            .any(|p| p["name"] == "X-Ardur-Cap-Token"),
        "an un-gated deployment must not advertise the header"
    );
    assert!(post["responses"]["403"].is_null());
}

// ---------------------------------------------------------------------------
// gh#470 — the gate and the receipt path must not diverge.
//
// Both defects below leave an AUTHORIZED mutation without its signed receipt:
// the decision persists, the route returns 200, and only an error log line
// records that the audit chain lost an entry. The receipt is the reason the
// #417 slice exists, so "the gate accepted it but the receipt path refused
// it" is a defect even though the mutation itself was authorized.
// ---------------------------------------------------------------------------

/// Mint a serialized cap-token with an explicit `budget_remaining`.
///
/// gh#470 R2: the route gate verifies `approval.decide` at cost 0 (admin
/// mutations are not metered), but the receipt path re-verifies the SAME
/// token at `cost_units` (default 1). A token issued with
/// `budget_remaining: 0` is legitimately issued (cost 0 <= budget 0) and
/// legitimately accepted by the gate, yet can never mint its receipt. The
/// verifier rejects it deterministically — not a race, not a clock skew: a
/// structurally unreachable receipt.
fn mint_with_budget(
    keypair: KeyPair,
    verbs: &[&str],
    expires_unix: u64,
    budget_remaining: u64,
) -> String {
    BiscuitCapTokenIssuer::new(keypair)
        .issue(
            CapHolderId(HOLDER.to_string()),
            CapScope {
                audience: AUDIENCE.to_string(),
                expires_unix,
                budget_remaining,
                tool_allowlist: verbs.iter().map(|v| (*v).to_string()).collect(),
            },
        )
        .expect("a token is issued")
        .to_base64()
        .expect("the token serializes")
}

/// R2: a zero-budget token passes the gate at cost 0 AND mints its receipt.
///
/// RED today: the receipt half fails because `commit_control_receipt`
/// re-verifies at `cost_units` (1), and 1 > 0 budget. The mutation succeeds
/// (200, card approved) while the receipt chain stays empty — the exact
/// silent audit gap #417 exists to close.
#[tokio::test]
async fn a_zero_budget_cap_token_still_mints_its_decision_receipt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-r2-zero-budget";
    seed_pending(dir.path(), id);

    let mut config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    config.admin_cap_token_gate = true;
    let router = support::boot_router(&config).await;
    let keypair = server_issuer_keypair(&config.data_dir);

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header(
            "X-Ardur-Cap-Token",
            mint_with_budget(keypair, &[APPROVAL_DECIDE_VERB], far_future(), 0),
        )
        .body(Body::empty())
        .expect("request builds");
    let (status, _) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK, "the gate admits cost-0 at budget 0");

    let chain =
        ardur_fused_runtime::load_persisted_chain(dir.path().join("receipts").join("chain.jsonl"))
            .expect("chain loads");
    assert_eq!(
        chain.len(),
        1,
        "the decision that passed the gate must produce its signed receipt"
    );
    assert_eq!(
        chain[0].body.subject.0, HOLDER,
        "the receipt binds to the presented capability's holder"
    );
}

/// R1: with the gate OFF, a presented token is NOT forwarded for receipt
/// binding — the gateway mints its own scoped token instead.
///
/// RED today: `presented_cap_token(&headers)` forwards ANY non-empty header
/// unconditionally, so `handle_approval_receipt` prefers the presented
/// string. A client that sends garbage alongside a valid bearer gets its
/// decision persisted while the receipt mint fails (`not-a-biscuit`), the
/// route returns 200, and the chain loses the entry. When the gate is off
/// there is no verification step the presented token passed, so forwarding
/// it converts an unverified header into a receipt-mint failure.
///
/// The contrast in `without_the_gate_the_receipt_keeps_the_gateway_subject`
/// (no header at all) is unchanged by this fix.
#[tokio::test]
async fn without_the_gate_a_presented_cap_token_is_not_forwarded_for_receipt_binding() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-r1-gate-off-forwarded";
    seed_pending(dir.path(), id);

    // Gate OFF — the default.
    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config).await;

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header("X-Ardur-Cap-Token", "not-a-biscuit")
        .body(Body::empty())
        .expect("request builds");
    let (status, _) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);

    let chain =
        ardur_fused_runtime::load_persisted_chain(dir.path().join("receipts").join("chain.jsonl"))
            .expect("chain loads");
    assert_eq!(
        chain.len(),
        1,
        "an un-gated decision must still mint its gateway-subject receipt"
    );
    assert_ne!(
        chain[0].body.subject.0, HOLDER,
        "the receipt must fall back to the gateway subject, not bind to an \
         unverified header the gate never checked"
    );
}

/// R1 (gated side of the same coin): with the gate ON, an invalid token in
/// the header never reaches the mutation — pinned by existing tests — but a
/// VALID presented token must still bind the receipt to its holder. Guards
/// against over-correction: suppressing the header entirely would regress
/// `the_receipt_binds_to_the_presented_capability`.
///
/// This test is expected GREEN before the fix (it is the existing pinned
/// behaviour); it exists to catch the naive fix for R1 (dropping the
/// header everywhere) breaking the gated binding.
#[tokio::test]
async fn with_the_gate_on_a_presented_token_still_binds_the_receipt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-r1-gate-on-binding";
    seed_pending(dir.path(), id);

    let mut config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    config.admin_cap_token_gate = true;
    let router = support::boot_router(&config).await;
    let keypair = server_issuer_keypair(&config.data_dir);

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/approve"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header(
            "X-Ardur-Cap-Token",
            mint(keypair, &[APPROVAL_DECIDE_VERB], far_future()),
        )
        .body(Body::empty())
        .expect("request builds");
    let (status, _) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);

    let chain =
        ardur_fused_runtime::load_persisted_chain(dir.path().join("receipts").join("chain.jsonl"))
            .expect("chain loads");
    assert_eq!(chain.len(), 1);
    assert_eq!(
        chain[0].body.subject.0, HOLDER,
        "gated decisions bind to the presented capability's holder"
    );
}

// ---- review suggestion 2 (re-review round 3): the added cases exercised the
// approve endpoint only. Mirror them on REJECT so the same guarantees hold on
// both decision verbs.

/// Zero-budget acceptance on REJECT: the gate admits cost-0, and the decision
/// receipt mints (the #470 defect on the reject path).
#[tokio::test]
async fn a_zero_budget_cap_token_still_mints_its_reject_receipt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-r2-zero-budget-reject";
    seed_pending(dir.path(), id);

    let mut config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    config.admin_cap_token_gate = true;
    let router = support::boot_router(&config).await;
    let keypair = server_issuer_keypair(&config.data_dir);

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/reject"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header(
            "X-Ardur-Cap-Token",
            mint_with_budget(keypair, &[APPROVAL_DECIDE_VERB], far_future(), 0),
        )
        .body(Body::empty())
        .expect("request builds");
    let (status, _) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK, "the gate admits cost-0 at budget 0");

    let chain =
        ardur_fused_runtime::load_persisted_chain(dir.path().join("receipts").join("chain.jsonl"))
            .expect("chain loads");
    assert_eq!(
        chain.len(),
        1,
        "the reject decision that passed the gate must produce its signed receipt"
    );
    assert_eq!(
        chain[0].body.subject.0, HOLDER,
        "the reject receipt binds to the presented capability's holder"
    );
}

/// R1 on REJECT, malformed header side: with the gate OFF, a garbage presented
/// token is not forwarded — the gateway mints its own scoped receipt.
#[tokio::test]
async fn reject_without_the_gate_a_malformed_cap_token_is_not_forwarded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-r1-reject-gate-off-malformed";
    seed_pending(dir.path(), id);

    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config).await;

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/reject"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header("X-Ardur-Cap-Token", "not-a-biscuit")
        .body(Body::empty())
        .expect("request builds");
    let (status, _) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);

    let chain =
        ardur_fused_runtime::load_persisted_chain(dir.path().join("receipts").join("chain.jsonl"))
            .expect("chain loads");
    assert_eq!(chain.len(), 1, "an un-gated reject still mints a receipt");
    assert_ne!(
        chain[0].body.subject.0, HOLDER,
        "the reject receipt falls back to the gateway subject, not an \\\\
         unverified header the gate never checked"
    );
}

/// R1 on REJECT, valid-header side: gate OFF + a VALID token still mints the
/// gateway-subject receipt (no verification ran, so the header is data).
#[tokio::test]
async fn reject_without_the_gate_a_valid_cap_token_is_not_forwarded_either() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-r1-reject-gate-off-valid";
    seed_pending(dir.path(), id);

    let config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    let router = support::boot_router(&config).await;
    let keypair = server_issuer_keypair(&config.data_dir);

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/reject"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header(
            "X-Ardur-Cap-Token",
            mint(keypair, &[APPROVAL_DECIDE_VERB], far_future()),
        )
        .body(Body::empty())
        .expect("request builds");
    let (status, _) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);

    let chain =
        ardur_fused_runtime::load_persisted_chain(dir.path().join("receipts").join("chain.jsonl"))
            .expect("chain loads");
    assert_eq!(chain.len(), 1);
    assert_ne!(
        chain[0].body.subject.0, HOLDER,
        "no gate verified the token, so the receipt must keep the gateway \\\\
         subject even for a well-formed header"
    );
}

/// Gate ON + REJECT + a token WITHOUT the approval verb: refused, nothing
/// mutates, no receipt.
#[tokio::test]
async fn reject_with_the_gate_a_token_without_the_verb_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-gate-on-reject-no-verb";
    seed_pending(dir.path(), id);

    let mut config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    config.admin_cap_token_gate = true;
    let router = support::boot_router(&config).await;
    let keypair = server_issuer_keypair(&config.data_dir);

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/reject"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header(
            "X-Ardur-Cap-Token",
            mint(keypair, &["some.other.verb"], far_future()),
        )
        .body(Body::empty())
        .expect("request builds");
    let (status, _) = support::oneshot(router, request).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "the gate refuses the wrong verb"
    );

    let chain_path = dir.path().join("receipts").join("chain.jsonl");
    if chain_path.exists() {
        let chain = ardur_fused_runtime::load_persisted_chain(&chain_path).expect("chain loads");
        assert!(chain.is_empty(), "a refused reject must mint no receipt");
    }
}

/// Gate ON + REJECT + valid token: the receipt binds to the holder.
#[tokio::test]
async fn reject_with_the_gate_a_presented_token_still_binds_the_receipt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let id = "card-gate-on-reject-binding";
    seed_pending(dir.path(), id);

    let mut config = support::test_config_with_admin(&dir, None, vec![ADMIN_TOKEN.to_string()]);
    config.admin_cap_token_gate = true;
    let router = support::boot_router(&config).await;
    let keypair = server_issuer_keypair(&config.data_dir);

    let request = Request::builder()
        .method("POST")
        .uri(format!("/approvals/{id}/reject"))
        .header("Authorization", format!("Bearer {ADMIN_TOKEN}"))
        .header(
            "X-Ardur-Cap-Token",
            mint(keypair, &[APPROVAL_DECIDE_VERB], far_future()),
        )
        .body(Body::empty())
        .expect("request builds");
    let (status, _) = support::oneshot(router, request).await;
    assert_eq!(status, StatusCode::OK);

    let chain =
        ardur_fused_runtime::load_persisted_chain(dir.path().join("receipts").join("chain.jsonl"))
            .expect("chain loads");
    assert_eq!(chain.len(), 1);
    assert_eq!(
        chain[0].body.subject.0, HOLDER,
        "a gated reject binds to the presented capability's holder"
    );
}
