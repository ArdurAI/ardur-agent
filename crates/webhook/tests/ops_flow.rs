//! End-to-end tests for the operator webhook surface (§9.7).
//!
//! Exercises the real gating substrate: Biscuit cap-tokens verified by
//! [`CapGate`], receipts signed with a real ES256 key and verified back through
//! the receipt chain, and a network-free mock [`Dispatcher`].

use std::sync::Mutex;

use ardur_cap_token::{BiscuitCapTokenIssuer, CapScope, CapTokenIssuer, HolderId, KeyPair};
use ardur_receipt::{Es256SigningKey, Jwks, ReceiptVerifier, Sha256Digest};
use ardur_webhook::{
    CapGate, DispatchRequest, DispatchResult, Dispatcher, EndpointRegistration, EndpointUpdate,
    Es256ReceiptSink, InMemoryReceiptSink, JsonCollectionStore, Principal, TriggerRegistration,
    WebhookError, WebhookOps, sign_body,
};
use secrecy::SecretString;

const AUDIENCE: &str = "webhook-ops";
const FUTURE: u64 = 4_102_444_800;
const NOW: u64 = 1_752_000_000;

fn issuer_and_gate() -> (BiscuitCapTokenIssuer, CapGate) {
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let gate = CapGate::new(issuer.public_key(), AUDIENCE);
    (issuer, gate)
}

fn mint(issuer: &BiscuitCapTokenIssuer, subject: &str, scopes: &[&str]) -> String {
    issuer
        .issue(
            HolderId(subject.to_string()),
            CapScope {
                audience: AUDIENCE.to_string(),
                expires_unix: FUTURE,
                budget_remaining: 1000,
                tool_allowlist: scopes.iter().map(|s| s.to_string()).collect(),
            },
        )
        .expect("issue")
        .to_base64()
        .expect("b64")
}

fn mem_ops(dir: &std::path::Path) -> WebhookOps<InMemoryReceiptSink> {
    let endpoints = JsonCollectionStore::new(dir.join("endpoints.json"));
    let triggers = JsonCollectionStore::new(dir.join("triggers.json"));
    WebhookOps::new(endpoints, triggers, InMemoryReceiptSink::new())
}

/// A network-free dispatcher that returns a configured status and captures the
/// last request for assertions.
struct MockDispatcher {
    status: u16,
    fail: bool,
    last: Mutex<Option<DispatchRequest>>,
}

impl MockDispatcher {
    fn ok(status: u16) -> Self {
        Self {
            status,
            fail: false,
            last: Mutex::new(None),
        }
    }
    fn transport_error() -> Self {
        Self {
            status: 0,
            fail: true,
            last: Mutex::new(None),
        }
    }
}

impl Dispatcher for MockDispatcher {
    fn dispatch(&self, request: &DispatchRequest) -> Result<DispatchResult, WebhookError> {
        *self.last.lock().unwrap() = Some(request.clone());
        if self.fail {
            Err(WebhookError::OutboundRequestFailed(
                "connection refused".into(),
            ))
        } else {
            Ok(DispatchResult {
                status: self.status,
            })
        }
    }
}

fn reg(name: &str, url: &str, secret_env: &str) -> EndpointRegistration {
    EndpointRegistration {
        name: name.to_string(),
        url: url.to_string(),
        method: None,
        secret_env: secret_env.to_string(),
        signature_header: None,
    }
}

#[test]
fn read_only_token_cannot_register() {
    let (issuer, gate) = issuer_and_gate();
    let dir = tempfile::tempdir().unwrap();
    let ops = mem_ops(dir.path());
    let principal = gate
        .authorize(
            &mint(&issuer, "cli://alice", &["webhook.endpoint.read"]),
            NOW,
        )
        .unwrap();

    let err = ops
        .register_endpoint(&principal, reg("hook", "https://example.com/h", "SECRET"))
        .expect_err("register must be refused without register scope");
    assert!(matches!(err, WebhookError::Denied(_)));
}

#[test]
fn register_list_update_revoke_flow() {
    let (issuer, gate) = issuer_and_gate();
    let dir = tempfile::tempdir().unwrap();
    let ops = mem_ops(dir.path());
    let principal = gate
        .authorize(
            &mint(
                &issuer,
                "cli://alice",
                &["webhook.endpoint.read", "webhook.endpoint.register"],
            ),
            NOW,
        )
        .unwrap();

    let id = ops
        .register_endpoint(
            &principal,
            reg("hook", "https://example.com/h", "SECRET_ENV"),
        )
        .expect("register");

    let list = ops.list_endpoints(&principal).expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, id);
    assert_eq!(list[0].signature_header, "X-Ardur-Webhook-Signature");

    ops.update_endpoint(
        &principal,
        &id,
        EndpointUpdate {
            url: Some("https://example.com/h2".to_string()),
            ..Default::default()
        },
    )
    .expect("update");
    assert_eq!(
        ops.get_endpoint(&principal, &id).unwrap().url,
        "https://example.com/h2"
    );

    ops.revoke_endpoint(&principal, &id).expect("revoke");
    assert!(ops.get_endpoint(&principal, &id).unwrap().revoked);

    // Receipt verbs cover the whole CRUD path.
    let verbs: Vec<String> = ops
        .receipt_sink()
        .events()
        .into_iter()
        .map(|e| e.verb)
        .collect();
    for expected in [
        "webhook.endpoint.registered.v1",
        "webhook.endpoint.listed.v1",
        "webhook.endpoint.updated.v1",
        "webhook.endpoint.revoked.v1",
    ] {
        assert!(verbs.contains(&expected.to_string()), "missing {expected}");
    }
}

#[test]
fn owner_scoping_hides_other_operators_endpoints() {
    let (issuer, gate) = issuer_and_gate();
    let dir = tempfile::tempdir().unwrap();
    let ops = mem_ops(dir.path());
    let scopes = &["webhook.endpoint.read", "webhook.endpoint.register"];
    let alice = gate
        .authorize(&mint(&issuer, "cli://alice", scopes), NOW)
        .unwrap();
    let bob = gate
        .authorize(&mint(&issuer, "cli://bob", scopes), NOW)
        .unwrap();

    let alice_id = ops
        .register_endpoint(&alice, reg("a", "https://a.example/h", "S"))
        .unwrap();
    ops.register_endpoint(&bob, reg("b", "https://b.example/h", "S"))
        .unwrap();

    assert_eq!(ops.list_endpoints(&alice).unwrap().len(), 1);
    // Bob cannot read or revoke Alice's endpoint.
    assert!(matches!(
        ops.get_endpoint(&bob, &alice_id),
        Err(WebhookError::Denied(_))
    ));
    assert!(matches!(
        ops.revoke_endpoint(&bob, &alice_id),
        Err(WebhookError::Denied(_))
    ));
}

#[test]
fn emit_signs_body_and_reports_delivered_on_2xx() {
    let (issuer, gate) = issuer_and_gate();
    let dir = tempfile::tempdir().unwrap();
    let ops = mem_ops(dir.path());
    let principal = gate
        .authorize(
            &mint(
                &issuer,
                "cli://alice",
                &[
                    "webhook.endpoint.read",
                    "webhook.endpoint.register",
                    "webhook.outbound.emit",
                ],
            ),
            NOW,
        )
        .unwrap();

    let secret_env = "ARDUR_TEST_WEBHOOK_SECRET_DELIVER";
    // SAFETY: integration-test process; unique var name per test avoids races.
    unsafe { std::env::set_var(secret_env, "top-secret-key") };

    let id = ops
        .register_endpoint(&principal, reg("hook", "https://example.com/h", secret_env))
        .unwrap();

    let payload = br#"{"event":"done"}"#;
    let dispatcher = MockDispatcher::ok(200);
    let report = ops
        .emit(&principal, &id, payload, &dispatcher)
        .expect("emit");
    assert!(report.delivered);
    assert_eq!(report.status, Some(200));

    // The dispatched body was signed with the resolved secret.
    let req = dispatcher.last.lock().unwrap().clone().unwrap();
    let expected = sign_body(payload, &SecretString::new("top-secret-key".into())).unwrap();
    let sig_header = req
        .headers
        .iter()
        .find(|(k, _)| k == "X-Ardur-Webhook-Signature")
        .map(|(_, v)| v.clone())
        .unwrap();
    assert_eq!(sig_header, format!("sha256={expected}"));
    assert!(req.headers.iter().any(|(k, _)| k == "X-Ardur-Emit-Nonce"));
    assert!(
        req.headers
            .iter()
            .any(|(k, _)| k == "X-Ardur-Idempotency-Key")
    );

    let verbs: Vec<String> = ops
        .receipt_sink()
        .events()
        .into_iter()
        .map(|e| e.verb)
        .collect();
    assert!(verbs.contains(&"webhook.outbound.attempted.v1".to_string()));
    assert!(verbs.contains(&"webhook.outbound.delivered.v1".to_string()));
}

#[test]
fn emit_reports_failed_on_5xx_and_transport_error() {
    let (issuer, gate) = issuer_and_gate();
    let dir = tempfile::tempdir().unwrap();
    let ops = mem_ops(dir.path());
    let principal = gate
        .authorize(
            &mint(
                &issuer,
                "cli://alice",
                &[
                    "webhook.endpoint.read",
                    "webhook.endpoint.register",
                    "webhook.outbound.emit",
                ],
            ),
            NOW,
        )
        .unwrap();

    let secret_env = "ARDUR_TEST_WEBHOOK_SECRET_FAIL";
    // SAFETY: integration-test process; unique var name per test avoids races.
    unsafe { std::env::set_var(secret_env, "k") };
    let id = ops
        .register_endpoint(&principal, reg("hook", "https://example.com/h", secret_env))
        .unwrap();

    let five = ops
        .emit(&principal, &id, b"{}", &MockDispatcher::ok(503))
        .unwrap();
    assert!(!five.delivered);
    assert_eq!(five.status, Some(503));

    let transport = ops
        .emit(&principal, &id, b"{}", &MockDispatcher::transport_error())
        .unwrap();
    assert!(!transport.delivered);
    assert_eq!(transport.status, None);
}

#[test]
fn emit_without_scope_is_refused() {
    let (issuer, gate) = issuer_and_gate();
    let dir = tempfile::tempdir().unwrap();
    let ops = mem_ops(dir.path());
    let principal = gate
        .authorize(
            &mint(
                &issuer,
                "cli://alice",
                &["webhook.endpoint.read", "webhook.endpoint.register"],
            ),
            NOW,
        )
        .unwrap();
    let secret_env = "ARDUR_TEST_WEBHOOK_SECRET_NOSCOPE";
    // SAFETY: integration-test process; unique var name per test avoids races.
    unsafe { std::env::set_var(secret_env, "k") };
    let id = ops
        .register_endpoint(&principal, reg("hook", "https://example.com/h", secret_env))
        .unwrap();
    assert!(matches!(
        ops.emit(&principal, &id, b"{}", &MockDispatcher::ok(200)),
        Err(WebhookError::Denied(_))
    ));
}

#[test]
fn emit_missing_secret_env_fails_to_resolve() {
    let (issuer, gate) = issuer_and_gate();
    let dir = tempfile::tempdir().unwrap();
    let ops = mem_ops(dir.path());
    let principal = gate
        .authorize(
            &mint(
                &issuer,
                "cli://alice",
                &[
                    "webhook.endpoint.read",
                    "webhook.endpoint.register",
                    "webhook.outbound.emit",
                ],
            ),
            NOW,
        )
        .unwrap();
    let id = ops
        .register_endpoint(
            &principal,
            reg("hook", "https://example.com/h", "ARDUR_TEST_ABSENT_VAR_XYZ"),
        )
        .unwrap();
    assert!(matches!(
        ops.emit(&principal, &id, b"{}", &MockDispatcher::ok(200)),
        Err(WebhookError::SigningKeyResolveFailed(_))
    ));
}

#[test]
fn secret_value_is_never_persisted_in_the_store() {
    let (issuer, gate) = issuer_and_gate();
    let dir = tempfile::tempdir().unwrap();
    let ops = mem_ops(dir.path());
    let principal = gate
        .authorize(
            &mint(
                &issuer,
                "cli://alice",
                &["webhook.endpoint.read", "webhook.endpoint.register"],
            ),
            NOW,
        )
        .unwrap();
    let secret_env = "ARDUR_TEST_WEBHOOK_SECRET_PERSIST";
    // SAFETY: integration-test process; unique var name per test avoids races.
    unsafe { std::env::set_var(secret_env, "MEGA-SECRET-VALUE-do-not-store") };
    ops.register_endpoint(&principal, reg("hook", "https://example.com/h", secret_env))
        .unwrap();

    let stored = std::fs::read_to_string(dir.path().join("endpoints.json")).unwrap();
    assert!(stored.contains(secret_env)); // the env-var *name* is stored
    assert!(!stored.contains("MEGA-SECRET-VALUE-do-not-store")); // the value is not
}

#[test]
fn trigger_register_list_remove_flow() {
    let (issuer, gate) = issuer_and_gate();
    let dir = tempfile::tempdir().unwrap();
    let ops = mem_ops(dir.path());
    let principal = gate
        .authorize(
            &mint(
                &issuer,
                "cli://alice",
                &["webhook.endpoint.read", "webhook.inbound.register"],
            ),
            NOW,
        )
        .unwrap();

    let id = ops
        .register_trigger(
            &principal,
            TriggerRegistration {
                name: "github".to_string(),
                path: "/hooks/github".to_string(),
                source: "github".to_string(),
                secret_env: "GH_HOOK_SECRET".to_string(),
                action: "run-ci-summary".to_string(),
                replay_window_secs: None,
            },
        )
        .expect("register trigger");

    assert_eq!(ops.list_triggers(&principal).unwrap().len(), 1);
    ops.remove_trigger(&principal, &id).expect("remove");
    assert!(ops.list_triggers(&principal).unwrap().is_empty());

    // A non-absolute path is rejected.
    assert!(matches!(
        ops.register_trigger(
            &principal,
            TriggerRegistration {
                name: "bad".to_string(),
                path: "no-slash".to_string(),
                source: "x".to_string(),
                secret_env: "S".to_string(),
                action: "a".to_string(),
                replay_window_secs: None,
            },
        ),
        Err(WebhookError::InvalidEndpoint(_))
    ));
}

#[test]
fn receipt_log_forms_a_verifiable_chain() {
    let (issuer, gate) = issuer_and_gate();
    let dir = tempfile::tempdir().unwrap();

    // Build the ops with a key we control so we can fully verify signatures.
    let key = Es256SigningKey::generate();
    let jwks = Jwks::from_public_key(&key.public_key());
    let log_path = dir.path().join("receipts").join("webhook.jsonl");
    let ops = WebhookOps::new(
        JsonCollectionStore::new(dir.path().join("endpoints.json")),
        JsonCollectionStore::new(dir.path().join("triggers.json")),
        Es256ReceiptSink::new(key, &log_path),
    );

    let principal: Principal = gate
        .authorize(
            &mint(
                &issuer,
                "cli://alice",
                &[
                    "webhook.endpoint.read",
                    "webhook.endpoint.register",
                    "webhook.outbound.emit",
                ],
            ),
            NOW,
        )
        .unwrap();
    let secret_env = "ARDUR_TEST_WEBHOOK_SECRET_CHAIN";
    // SAFETY: integration-test process; unique var name per test avoids races.
    unsafe { std::env::set_var(secret_env, "k") };

    let id = ops
        .register_endpoint(&principal, reg("hook", "https://example.com/h", secret_env))
        .unwrap();
    ops.list_endpoints(&principal).unwrap();
    ops.emit(&principal, &id, b"{}", &MockDispatcher::ok(200))
        .unwrap();

    // Verify every JWS signature and the parent-hash linkage over the log.
    let text = std::fs::read_to_string(&log_path).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(
        lines.len() >= 4,
        "expected >= 4 receipts, got {}",
        lines.len()
    );

    let mut prev: Option<Sha256Digest> = None;
    for line in &lines {
        let verified = ReceiptVerifier::verify_compact(line, &jwks).expect("verify jws");
        assert_eq!(verified.body.parent_hash, prev, "hash chain must link");
        prev = Some(Sha256Digest::of(line.as_bytes()));
    }
}
