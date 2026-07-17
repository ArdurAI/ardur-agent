//! End-to-end tests for the cron operator controller (§9.4).
//!
//! These exercise the real gating substrate: cap-tokens minted by a Biscuit
//! issuer and verified by [`CapGate`], and receipts signed with a real ES256
//! key and verified back through the receipt chain.

use ardur_cap_token::{BiscuitCapTokenIssuer, CapScope, CapTokenIssuer, HolderId, KeyPair};
use ardur_cron_ui::{
    CapGate, CreateRequest, CronController, CronFilter, CronMutation, CronStatus, DeliveryMode,
    Es256ReceiptSink, InMemoryCronStore, InMemoryReceiptSink, Principal, Redactor, VisibilityTier,
    render_list, validate_cron,
};
use ardur_receipt::{Es256SigningKey, Jwks, ReceiptVerifier, Sha256Digest};

const AUDIENCE: &str = "ardur-cron-ui";
const FUTURE: u64 = 4_102_444_800; // 2100-01-01
const NOW: u64 = 1_752_000_000; // mid-2025-ish, well before FUTURE

fn issuer_and_gate() -> (BiscuitCapTokenIssuer, CapGate) {
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let gate = CapGate::new(issuer.public_key(), AUDIENCE);
    (issuer, gate)
}

fn mint(issuer: &BiscuitCapTokenIssuer, subject: &str, scopes: &[&str]) -> String {
    let token = issuer
        .issue(
            HolderId(subject.to_string()),
            CapScope {
                audience: AUDIENCE.to_string(),
                expires_unix: FUTURE,
                budget_remaining: 1000,
                tool_allowlist: scopes.iter().map(|s| s.to_string()).collect(),
            },
        )
        .expect("issue");
    token.to_base64().expect("to_base64")
}

fn create_req(name: &str) -> CreateRequest {
    CreateRequest {
        name: name.to_string(),
        schedule_expr: "0 9 * * 1".to_string(),
        prompt: "summarize the weekly report".to_string(),
        delivery_mode: DeliveryMode::InternalOnly,
        model_override: None,
        mission_tag: Some("reports".to_string()),
    }
}

#[test]
fn view_only_token_cannot_mutate() {
    let (issuer, gate) = issuer_and_gate();
    let view_token = mint(&issuer, "cli://alice", &["cron.ui.view"]);
    let principal = gate.authorize(&view_token, NOW).expect("authorize view");
    assert!(!principal.has("cron.ui.mutate"));

    let receipts = InMemoryReceiptSink::new();
    let controller = CronController::new(InMemoryCronStore::new(), receipts);

    let err = controller
        .mutate(&principal, CronMutation::Create(create_req("weekly")))
        .expect_err("view-only mutate must be refused");
    assert!(format!("{err}").contains("cron.ui.mutate"));

    // The attempt AND the refusal are both receipted (audit coverage).
    let verbs: Vec<String> = controller
        .store_receipts()
        .events()
        .into_iter()
        .map(|e| e.verb)
        .collect();
    assert!(verbs.contains(&"cron.create.attempted.v1".to_string()));
    assert!(verbs.contains(&"cron.mutate.refused.v1".to_string()));
}

#[test]
fn mutate_token_creates_pauses_and_deletes() {
    let (issuer, gate) = issuer_and_gate();
    let token = mint(&issuer, "cli://alice", &["cron.ui.view", "cron.ui.mutate"]);
    let principal = gate.authorize(&token, NOW).expect("authorize");

    let controller = CronController::new(InMemoryCronStore::new(), InMemoryReceiptSink::new());

    let report = controller
        .mutate(&principal, CronMutation::Create(create_req("weekly")))
        .expect("create");
    assert!(report.success);
    assert!(report.receipt_id.is_some());
    let id = report.cron_id.clone();

    // Listing shows the new cron for its owner.
    let rows = controller
        .list(&principal, &CronFilter::All, VisibilityTier::SelfOnly, NOW)
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, CronStatus::Active);

    // Pause flips status.
    controller
        .mutate(&principal, CronMutation::Pause(id.clone()))
        .expect("pause");
    let detail = controller.detail(&principal, &id).expect("detail");
    assert_eq!(detail.row.status, CronStatus::Paused);

    // Resume flips it back.
    controller
        .mutate(&principal, CronMutation::Resume(id.clone()))
        .expect("resume");
    assert_eq!(
        controller
            .detail(&principal, &id)
            .expect("detail")
            .row
            .status,
        CronStatus::Active
    );

    // Delete removes it.
    controller
        .mutate(&principal, CronMutation::Delete(id.clone()))
        .expect("delete");
    let rows = controller
        .list(&principal, &CronFilter::All, VisibilityTier::SelfOnly, NOW)
        .expect("list");
    assert!(rows.is_empty());
}

#[test]
fn planted_secret_is_redacted_on_render() {
    let (issuer, gate) = issuer_and_gate();
    let token = mint(&issuer, "cli://alice", &["cron.ui.view", "cron.ui.mutate"]);
    let principal = gate.authorize(&token, NOW).expect("authorize");
    let controller = CronController::new(InMemoryCronStore::new(), InMemoryReceiptSink::new());

    let mut req = create_req("leaky");
    req.name = "job sk-ABCDEFGHIJKLMNOP01234567 here".to_string();
    req.prompt = "call api_key=supersecretvalue123".to_string();
    let report = controller
        .mutate(&principal, CronMutation::Create(req))
        .expect("create");

    let detail = controller
        .detail(&principal, &report.cron_id)
        .expect("detail");
    assert!(!detail.row.name.contains("sk-ABCDEFGHIJKLMNOP"));
    assert!(detail.row.name.contains("<redacted>"));
    assert!(!detail.prompt.contains("supersecretvalue123"));

    let rendered = render_list(
        &controller
            .list(&principal, &CronFilter::All, VisibilityTier::SelfOnly, NOW)
            .expect("list"),
        ardur_cron_ui::Density::Comfortable,
    );
    assert!(!rendered.contains("sk-ABCDEFGHIJKLMNOP"));
}

#[test]
fn self_visibility_hides_other_operators_but_admin_sees_all() {
    let (issuer, gate) = issuer_and_gate();
    let store = InMemoryCronStore::new();
    let controller = CronController::new(store, InMemoryReceiptSink::new());

    let alice = gate
        .authorize(
            &mint(&issuer, "cli://alice", &["cron.ui.view", "cron.ui.mutate"]),
            NOW,
        )
        .expect("alice");
    let bob = gate
        .authorize(
            &mint(&issuer, "cli://bob", &["cron.ui.view", "cron.ui.mutate"]),
            NOW,
        )
        .expect("bob");

    controller
        .mutate(&alice, CronMutation::Create(create_req("alice-job")))
        .expect("alice create");
    controller
        .mutate(&bob, CronMutation::Create(create_req("bob-job")))
        .expect("bob create");

    // Each operator sees only their own under SelfOnly.
    let alice_rows = controller
        .list(&alice, &CronFilter::All, VisibilityTier::SelfOnly, NOW)
        .expect("alice list");
    assert_eq!(alice_rows.len(), 1);

    // Bob cannot elevate to Tenant without the admin scope.
    let bob_tenant = controller.list(&bob, &CronFilter::All, VisibilityTier::Tenant, NOW);
    assert!(bob_tenant.is_err());

    // An admin sees everyone's crons under Tenant.
    let admin = gate
        .authorize(
            &mint(&issuer, "cli://root", &["cron.ui.view", "cron.ui.admin"]),
            NOW,
        )
        .expect("admin");
    let all = controller
        .list(&admin, &CronFilter::All, VisibilityTier::Tenant, NOW)
        .expect("admin list");
    assert_eq!(all.len(), 2);
}

#[test]
fn expired_token_is_refused() {
    let (issuer, gate) = issuer_and_gate();
    let token = issuer
        .issue(
            HolderId("cli://alice".to_string()),
            CapScope {
                audience: AUDIENCE.to_string(),
                expires_unix: NOW - 1,
                budget_remaining: 1000,
                tool_allowlist: vec!["cron.ui.view".to_string()],
            },
        )
        .expect("issue")
        .to_base64()
        .expect("b64");
    assert!(gate.authorize(&token, NOW).is_err());
}

#[test]
fn wrong_audience_is_refused() {
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let gate = CapGate::new(issuer.public_key(), "some-other-audience");
    let token = mint(&issuer, "cli://alice", &["cron.ui.view"]);
    assert!(gate.authorize(&token, NOW).is_err());
}

#[test]
fn receipt_log_forms_a_verifiable_chain() {
    let (issuer, gate) = issuer_and_gate();
    let token = mint(&issuer, "cli://alice", &["cron.ui.view", "cron.ui.mutate"]);
    let principal: Principal = gate.authorize(&token, NOW).expect("authorize");

    let dir = tempfile::tempdir().expect("tempdir");
    let log_path = dir.path().join("receipts").join("cron-ui.jsonl");
    let key = Es256SigningKey::generate();
    let jwks = Jwks::from_public_key(&key.public_key());
    let sink = Es256ReceiptSink::new(key, &log_path);
    let controller = CronController::new(InMemoryCronStore::new(), sink);

    // Several actions → several chained receipts.
    let report = controller
        .mutate(&principal, CronMutation::Create(create_req("weekly")))
        .expect("create");
    controller
        .mutate(&principal, CronMutation::Pause(report.cron_id.clone()))
        .expect("pause");
    controller
        .list(&principal, &CronFilter::All, VisibilityTier::SelfOnly, NOW)
        .expect("list");

    // Read the log and verify every JWS signature + hash linkage.
    let text = std::fs::read_to_string(&log_path).expect("read log");
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(
        lines.len() >= 4,
        "expected >= 4 receipts, got {}",
        lines.len()
    );

    let mut prev_hash: Option<Sha256Digest> = None;
    for line in &lines {
        let verified = ReceiptVerifier::verify_compact(line, &jwks).expect("verify jws");
        assert_eq!(
            verified.body.parent_hash, prev_hash,
            "chain linkage must hold"
        );
        prev_hash = Some(Sha256Digest::of(line.as_bytes()));
    }
}

#[test]
fn validate_cron_accepts_supported_grammar_and_rejects_junk() {
    assert!(validate_cron("* * * * *").is_ok());
    assert!(validate_cron("0,15,30,45 9-17 * * 1-5").is_ok());
    assert!(validate_cron("*/5 * * * *").is_ok());
    assert!(validate_cron("0 9 * *").is_err()); // too few fields
    assert!(validate_cron("0 9 * * MON").is_err()); // names unsupported by matcher
    assert!(validate_cron("bogus * * * *").is_err());
}

#[test]
fn filter_parse_and_compose() {
    let redactor = Redactor::new();
    let f = CronFilter::parse("status:errored", &redactor);
    assert!(matches!(f, CronFilter::Status(_)));
    let f = CronFilter::parse("tag:reports", &redactor);
    assert_eq!(f, CronFilter::MissionTag("reports".to_string()));
    // Free text with a planted secret is scanned before it becomes a query.
    let f = CronFilter::parse("sk-ABCDEFGHIJKLMNOP01234567", &redactor);
    match f {
        CronFilter::SearchText(t) => assert!(t.contains("<redacted>")),
        other => panic!("expected SearchText, got {other:?}"),
    }
}
