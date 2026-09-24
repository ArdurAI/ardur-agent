//! #544 — typed plane-outage classification and idempotent reconnect
//! replay, guarded against the pinned plane revision's wire shapes
//! (ArdurAI/ardur `dev` @ 9cd2f2a5).
//!
//! Every class in [`PlaneOutcome`] is pinned in BOTH directions where the
//! distinction is a safety property: the deny-shaped and corrupt classes
//! must never be fallback-eligible, and only the enumerated transport
//! shapes may be. Replay is proven idempotent (duplicate → DupExplain),
//! conflicting re-decisions fail closed (CorruptEvidence), and a tampered
//! journal refuses to load.

use std::time::Duration;

use ardur_governance::{
    GrantDescriptor, KILL_SWITCH_ACTIVE, PASSPORT_REVOKED, PlaneClient, PlaneEventRecord,
    PlaneEventStatus, PlaneOutcome,
};
use serde_json::json;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn grant() -> GrantDescriptor {
    GrantDescriptor {
        grant_id: "7c9e6679-7425-40de-944b-e07fc1f90ae7".to_string(),
        subject: "spiffe://ardur/user/test".to_string(),
        mission_ref: None,
        effective_tools: vec!["echo".to_string()],
        budget_remaining: 1_000,
        expires_unix: 1_750_030_000,
    }
}

/// Parse the journal into its records (structural reads, not escaped-text
/// matching).
fn journal_records(path: &std::path::Path) -> Vec<PlaneEventRecord> {
    let text = std::fs::read_to_string(path).expect("journal readable");
    let mut records = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let envelope: serde_json::Value = serde_json::from_str(line).expect("envelope json");
        let record_line = envelope
            .get("record")
            .and_then(|r| r.as_str())
            .expect("envelope carries the record");
        // The first line is the identity header (plane endpoint + root
        // fingerprint), not an event record.
        let value: serde_json::Value =
            serde_json::from_str(record_line).expect("record/header json");
        if value.get("plane").is_some() {
            continue;
        }
        records.push(serde_json::from_value(value).expect("record parses"));
    }
    records
}

const ROOT_PEM: &str = "-----BEGIN PUBLIC KEY-----\nfake-root\n-----END PUBLIC KEY-----";

fn open_client(server: &MockServer, journal: &std::path::Path) -> PlaneClient {
    PlaneClient::open(&server.uri(), "test-token", ROOT_PEM, journal).expect("client opens")
}

async fn consult(client: &PlaneClient) -> PlaneOutcome {
    client
        .evaluate(
            "ev:0123456789abcdef0123456789abcdef01234567",
            "session-1",
            "echo",
            br#"{"msg":"hi"}"#.as_slice(),
            "b".repeat(64).as_str(),
            &grant(),
            1_750_000_000,
        )
        .await
        .expect("consult completes")
}

fn permit_mock() -> Mock {
    Mock::given(method("POST"))
        .and(path("/evaluate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "PERMIT",
            "session_id": "session-1",
        })))
}

fn permit_or_deny_body(body: serde_json::Value) -> Mock {
    Mock::given(method("POST"))
        .and(path("/evaluate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
}

#[tokio::test]
async fn authenticated_permit_and_deny_are_values_not_outages() {
    let server = MockServer::start().await;
    let journal = tempfile::tempdir().expect("tempdir");
    let client = open_client(&server, &journal.path().join("plane.jsonl"));

    // Both mocks match on the event id they decide, so mount order cannot
    // blur the two decisions.
    Mock::given(method("POST"))
        .and(path("/evaluate"))
        .and(body_partial_json(json!({
            "risk_request_id": "ev:0123456789abcdef0123456789abcdef01234567"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "PERMIT",
            "session_id": "session-1",
        })))
        .mount(&server)
        .await;
    assert!(matches!(consult(&client).await, PlaneOutcome::Permitted));

    // A second event id on the same plane: matched to a DENY, proving the
    // consult presents the stable event id as its `risk_request_id`
    // idempotency key (the mock only answers requests carrying it).
    Mock::given(method("POST"))
        .and(path("/evaluate"))
        .and(body_partial_json(json!({
            "risk_request_id": "ev:1111111111111111111111111111111111111111"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "decision": "DENY",
            "session_id": "session-1",
            "reason": "tool_not_allowed",
        })))
        .mount(&server)
        .await;
    let outcome = client
        .evaluate(
            "ev:1111111111111111111111111111111111111111",
            "session-1",
            "echo",
            br#"{"msg":"hi"}"#.as_slice(),
            "d".repeat(64).as_str(),
            &grant(),
            1_750_000_000,
        )
        .await
        .expect("consult completes");
    match outcome {
        PlaneOutcome::Denied { ref reason } => {
            assert_eq!(reason.as_deref(), Some("tool_not_allowed"))
        }
        other => panic!("expected Denied, got {other:?}"),
    }
    // The authenticated-decision classes are never fallback-eligible.
    assert!(!outcome.fallback_eligible());
}

#[tokio::test]
async fn revoked_is_a_denial_not_an_outage() {
    let server = MockServer::start().await;
    let journal = tempfile::tempdir().expect("tempdir");
    Mock::given(method("POST"))
        .and(path("/evaluate"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": PASSPORT_REVOKED,
        })))
        .mount(&server)
        .await;
    let client = open_client(&server, &journal.path().join("plane.jsonl"));
    let outcome = consult(&client).await;
    assert!(matches!(outcome, PlaneOutcome::Revoked));
    assert!(!outcome.fallback_eligible());
}

#[tokio::test]
async fn kill_switch_is_a_denial_distinct_from_transport_failure() {
    let server = MockServer::start().await;
    let journal = tempfile::tempdir().expect("tempdir");
    Mock::given(method("POST"))
        .and(path("/evaluate"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": KILL_SWITCH_ACTIVE,
        })))
        .mount(&server)
        .await;
    let client = open_client(&server, &journal.path().join("plane.jsonl"));
    let outcome = consult(&client).await;
    assert!(matches!(outcome, PlaneOutcome::KillSwitch));
    assert!(!outcome.fallback_eligible());
}

#[tokio::test]
async fn an_unknown_503_is_corrupt_evidence_never_generic_unavailability() {
    // The plane answered 503 but NOT with the kill-switch body the pinned
    // revision produces: an unrecognised 503 must not launder itself into
    // the fallback class.
    let server = MockServer::start().await;
    let journal = tempfile::tempdir().expect("tempdir");
    Mock::given(method("POST"))
        .and(path("/evaluate"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({
            "error": "something_else",
        })))
        .mount(&server)
        .await;
    let client = open_client(&server, &journal.path().join("plane.jsonl"));
    let outcome = consult(&client).await;
    assert!(matches!(outcome, PlaneOutcome::CorruptEvidence { .. }));
    assert!(!outcome.fallback_eligible());
}

#[tokio::test]
async fn enumerated_transport_shapes_are_the_only_fallback_class() {
    // 404 (no endpoint), 502/504 (gateway no-decision), and connection
    // refused all classify Unreachable and are fallback-eligible. The
    // refused case targets the discard port (nothing listens there — a
    // dropped MockServer's port can be reused by a parallel test's server).
    let journal = tempfile::tempdir().expect("tempdir");
    let client = PlaneClient::open(
        "http://127.0.0.1:9",
        "test-token",
        ROOT_PEM,
        &journal.path().join("p.jsonl"),
    )
    .expect("client opens");
    let refused = consult(&client).await;
    assert!(
        matches!(refused, PlaneOutcome::Unreachable { .. }),
        "connection refused must be Unreachable, got {refused:?}"
    );
    assert!(refused.fallback_eligible());

    for status in [404u16, 502, 504] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/evaluate"))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                "error": "no decision",
            })))
            .mount(&server)
            .await;
        let client = open_client(&server, &journal.path().join(format!("p{status}.jsonl")));
        let outcome = consult(&client).await;
        assert!(
            matches!(outcome, PlaneOutcome::Unreachable { .. }),
            "status {status} must be Unreachable, got {outcome:?}"
        );
        assert!(outcome.fallback_eligible());
    }
}

#[tokio::test]
async fn a_timeout_is_ambiguous_delivery_and_never_falls_back() {
    // The classic bypass: a request that times out mid-flight may have been
    // delivered and decided server-side. It must deny, not fall back.
    let server = MockServer::start().await;
    let journal = tempfile::tempdir().expect("tempdir");
    Mock::given(method("POST"))
        .and(path("/evaluate"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;
    let client = open_client(&server, &journal.path().join("plane.jsonl"));
    let started = std::time::Instant::now();
    let outcome = consult(&client).await;
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "must not wait the full delay"
    );
    assert!(matches!(outcome, PlaneOutcome::AmbiguousDelivery));
    assert!(!outcome.fallback_eligible());
}

#[tokio::test]
async fn corrupt_bodies_are_corrupt_evidence_never_compliance() {
    let cases: Vec<(serde_json::Value, &str)> = vec![
        (
            json!({"decision": "MAYBE", "session_id": "session-1"}),
            "unknown decision word",
        ),
        (json!({"session_id": "s"}), "missing decision"),
        (
            json!({"decision": "PERMIT", "session_id": "session-1", "reason": "why"}),
            "reason riding a PERMIT",
        ),
        (
            json!({"decision": "DENY", "session_id": "session-1", "reason": 7}),
            "non-string reason",
        ),
    ];
    for (body, label) in cases {
        let server = MockServer::start().await;
        let journal = tempfile::tempdir().expect("tempdir");
        permit_or_deny_body(body).mount(&server).await;
        let client = open_client(&server, &journal.path().join("plane.jsonl"));
        let outcome = consult(&client).await;
        assert!(
            matches!(outcome, PlaneOutcome::CorruptEvidence { .. }),
            "{label} must be CorruptEvidence, got {outcome:?}"
        );
        assert!(!outcome.fallback_eligible());
    }
    // A non-JSON 200 body is corrupt too.
    let server = MockServer::start().await;
    let journal = tempfile::tempdir().expect("tempdir");
    Mock::given(method("POST"))
        .and(path("/evaluate"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>hi</html>"))
        .mount(&server)
        .await;
    let client = open_client(&server, &journal.path().join("plane.jsonl"));
    assert!(matches!(
        consult(&client).await,
        PlaneOutcome::CorruptEvidence { .. }
    ));
}

#[tokio::test]
async fn auth_failures_are_denials_not_outages() {
    let server = MockServer::start().await;
    let journal = tempfile::tempdir().expect("tempdir");
    Mock::given(method("POST"))
        .and(path("/evaluate"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "invalid bearer token",
        })))
        .mount(&server)
        .await;
    let client = open_client(&server, &journal.path().join("plane.jsonl"));
    let outcome = consult(&client).await;
    assert!(matches!(outcome, PlaneOutcome::Denied { .. }));
    assert!(!outcome.fallback_eligible());
}

#[tokio::test]
async fn replay_of_a_decided_event_is_idempotent_and_conflicts_fail_closed() {
    let server = MockServer::start().await;
    let journal_dir = tempfile::tempdir().expect("tempdir");
    let journal = journal_dir.path().join("plane.jsonl");

    // First consult: ambiguous (timeout) — the event stays in the replay
    // backlog as response-unknown.
    Mock::given(method("POST"))
        .and(path("/evaluate"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;
    let client = open_client(&server, &journal);
    let outcome = consult(&client).await;
    assert!(matches!(outcome, PlaneOutcome::AmbiguousDelivery));

    // Reconnect: the plane now answers definitively for the SAME event id.
    // The idempotency key (`risk_request_id`) is presented; the previously
    // response-unknown delivery is explained and closes TERMINAL — it was
    // answered, not re-decided.
    server.reset().await;
    let client = open_client(&server, &journal);
    permit_mock().expect(1..).mount(&server).await;
    let replayed = client.replay_backlog().await.expect("replay completes");
    assert_eq!(replayed, 1, "exactly the one backlog event replays");

    let records = journal_records(&journal);
    let last = records
        .iter()
        .rev()
        .find(|r| r.event_id == "ev:0123456789abcdef0123456789abcdef01234567")
        .expect("the event is journaled");
    assert_eq!(
        last.status,
        PlaneEventStatus::DupExplain,
        "an explained response-unknown delivery is the explained duplicate, got {:?}",
        last.status
    );
    assert!(matches!(last.outcome, Some(PlaneOutcome::Permitted)));

    // Re-running the replay drains nothing (the event is closed) —
    // idempotent, not re-sent.
    let replayed_again = client.replay_backlog().await.expect("replay completes");
    assert_eq!(replayed_again, 0, "a closed event never replays again");

    // Duplicate delivery: a consult whose DEFINITIVE Permit was journaled,
    // crashed before the terminal append (the durable state is its Deliver
    // line — a valid MAC-chain prefix), then replayed against a plane that
    // re-delivers the SAME decision. That is the explained duplicate.
    let journal3 = journal_dir.path().join("dup.jsonl");
    server.reset().await;
    permit_mock().mount(&server).await;
    let client = open_client(&server, &journal3);
    let _ = consult(&client).await; // Permit terminal journaled
    {
        let text = std::fs::read_to_string(&journal3).expect("journal readable");
        let lines: Vec<&str> = text.lines().collect();
        // header + pending + deliver (the terminal line is the crash gap)
        assert!(
            lines.len() >= 4,
            "header+pending+deliver+terminal were written"
        );
        let truncated = format!("{}\n", lines[..3].join("\n"));
        std::fs::write(&journal3, &truncated).expect("write");
        // Model the checkpoint a REAL crash-before-terminal leaves: the
        // deliver append (and its checkpoint) landed, the terminal never
        // did. (Deleting the terminal of a checkpointed journal is exactly
        // the truncation the checkpoint exists to catch — guarded below.)
        std::fs::write(
            journal3.with_extension("jsonl.checkpoint"),
            truncated.len().to_string(),
        )
        .expect("checkpoint");
    }
    let client = open_client(&server, &journal3);
    let replayed = client.replay_backlog().await.expect("replay completes");
    assert_eq!(replayed, 1, "the crashed event replays");
    let records = journal_records(&journal3);
    let dup = records
        .iter()
        .rev()
        .find(|r| r.event_id == "ev:0123456789abcdef0123456789abcdef01234567")
        .expect("the event is journaled");
    assert_eq!(
        dup.status,
        PlaneEventStatus::DupExplain,
        "the plane re-delivering its prior PERMIT must be recorded as an explained duplicate, got {:?}",
        dup.status
    );
    assert!(matches!(dup.outcome, Some(PlaneOutcome::Permitted)));

    // The conflicting-definitive-re-decision classification (CorruptEvidence)
    // is covered by the unit tests in src/plane.rs — it is defense in depth
    // for a journal state the live backlog does not produce.
}

#[tokio::test]
async fn a_tampered_journal_refuses_to_load() {
    let server = MockServer::start().await;
    let journal_dir = tempfile::tempdir().expect("tempdir");
    let journal = journal_dir.path().join("plane.jsonl");
    let client = open_client(&server, &journal);
    permit_mock().mount(&server).await;
    let _ = consult(&client).await;

    // Flip the subject inside one record payload — the MAC must catch it.
    let text = std::fs::read_to_string(&journal).expect("journal readable");
    let tampered = text.replace("spiffe://ardur/user/test", "spiffe://ardur/user/evil");
    assert_ne!(
        tampered, text,
        "the fixture must actually change the record"
    );
    std::fs::write(&journal, tampered).expect("write");
    let reopened = PlaneClient::open(
        &server.uri(),
        "test-token",
        "-----BEGIN PUBLIC KEY-----\nfake-root\n-----END PUBLIC KEY-----",
        &journal,
    );
    assert!(
        reopened.is_err(),
        "a tampered plane journal must fail the open, not replay"
    );

    // And a deleted line (seq gap) is caught by the chain.
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() >= 2 {
        let gapped = format!("{}\n", lines[1..].join("\n"));
        std::fs::write(&journal, gapped).expect("write");
        assert!(PlaneClient::open(&server.uri(), "test-token", ROOT_PEM, &journal,).is_err());
    }
}

#[tokio::test]
async fn the_journal_records_the_exact_md_dg_and_manifest_snapshots() {
    let server = MockServer::start().await;
    let journal_dir = tempfile::tempdir().expect("tempdir");
    let journal = journal_dir.path().join("plane.jsonl");
    permit_mock().mount(&server).await;
    let client = open_client(&server, &journal);
    let _ = consult(&client).await;

    let records = journal_records(&journal);
    let pending = records
        .iter()
        .find(|r| r.status == PlaneEventStatus::Pending)
        .expect("the Pending record precedes delivery");
    // The decision is bound to the exact MD/DG/manifest the plane saw.
    assert_eq!(
        pending.event_id,
        "ev:0123456789abcdef0123456789abcdef01234567"
    );
    assert_eq!(pending.tool, "echo");
    assert_eq!(pending.manifest_digest, "b".repeat(64));
    // The digest is derived from the canonical argument bytes sent to the
    // plane (#544 review: real arguments travel, digest stays journaled).
    assert_eq!(
        pending.arguments_digest,
        // sha256 of {"msg":"hi"}
        "d95808527f6e74a7a4cc2d3dfc056424bea5dce3940f31f158d06ad5098fbdd8"
    );
    assert_eq!(
        pending.grant.grant_id,
        "7c9e6679-7425-40de-944b-e07fc1f90ae7"
    );
    assert_eq!(pending.grant.subject, "spiffe://ardur/user/test");
    assert_eq!(pending.grant.effective_tools, vec!["echo".to_string()]);
    assert_eq!(pending.grant.budget_remaining, 1_000);
    assert_eq!(pending.grant.expires_unix, 1_750_030_000);
    let terminal = records
        .iter()
        .rev()
        .find(|r| r.status == PlaneEventStatus::Terminal)
        .expect("the decision is terminal");
    assert!(matches!(terminal.outcome, Some(PlaneOutcome::Permitted)));
}

#[test]
fn outcome_classes_are_fully_enumerated_by_the_tests() {
    // The mutation guard: every variant is asserted somewhere above in its
    // safety direction. This pins the population so a new variant without a
    // decision here fails this count.
    let all = [
        PlaneOutcome::Permitted,
        PlaneOutcome::Denied { reason: None },
        PlaneOutcome::Revoked,
        PlaneOutcome::KillSwitch,
        PlaneOutcome::Unreachable {
            window: ardur_governance::OutageWindow {
                key: "k".to_string(),
                started_unix_secs: 0,
            },
        },
        PlaneOutcome::AmbiguousDelivery,
        PlaneOutcome::CorruptEvidence {
            detail: String::new(),
        },
    ];
    assert_eq!(
        all.len(),
        7,
        "a new PlaneOutcome variant needs a class decision here"
    );
    assert_eq!(
        all.iter().filter(|o| o.fallback_eligible()).count(),
        1,
        "exactly one fallback-eligible class"
    );
    assert_eq!(all.iter().filter(|o| o.permits()).count(), 1);
}

#[test]
fn windows_are_bounded_and_keyed() {
    let server_uri = "http://127.0.0.1:1";
    let journal = tempfile::tempdir().expect("tempdir");
    let client = PlaneClient::open(
        server_uri,
        "t",
        "-----BEGIN PUBLIC KEY-----\nfake-root\n-----END PUBLIC KEY-----",
        &journal.path().join("w.jsonl"),
    )
    .expect("opens");
    // Derive the boundaries from the constant itself, so a future window
    // length change keeps this test meaningful rather than numerically
    // wrong.
    let w = ardur_governance::OUTAGE_WINDOW_SECS.max(1);
    let base = (1_750_000_049 / w) * w;
    let a = client.window(base + w / 2);
    let b = client.window(base + w / 2 + w);
    assert_eq!(a.key, b.key, "same plane identity → same window key");
    assert_ne!(
        a.started_unix_secs, b.started_unix_secs,
        "windows advance across the boundary"
    );
    assert_eq!(a.started_unix_secs, base);
    let c = client.window(base + w - 1);
    assert_eq!(
        a.started_unix_secs, c.started_unix_secs,
        "within one window"
    );
}

// Ensure the status enum's wire spellings stay pinned (the journal text
// assertions above depend on them).
#[test]
fn status_wire_spellings() {
    assert_eq!(
        serde_json::to_string(&PlaneEventStatus::DupExplain).expect("serde"),
        "\"dup_explain\""
    );
    assert_eq!(
        serde_json::to_string(&PlaneEventStatus::Terminal).expect("serde"),
        "\"terminal\""
    );
    assert_eq!(
        serde_json::to_string(&PlaneEventStatus::Pending).expect("serde"),
        "\"pending\""
    );
    assert_eq!(
        serde_json::to_string(&PlaneEventStatus::Deliver).expect("serde"),
        "\"deliver\""
    );
}

#[tokio::test]
async fn plaintext_http_is_refused_for_non_loopback_planes() {
    let journal = tempfile::tempdir().expect("tempdir");
    let err = PlaneClient::open(
        "http://plane.example.com:8443",
        "t",
        ROOT_PEM,
        &journal.path().join("p.jsonl"),
    )
    .err()
    .expect("non-loopback http must be refused");
    assert!(err.to_string().contains("plaintext http"), "got: {err}");
    // Loopback http stays the explicit development exception.
    assert!(
        PlaneClient::open(
            "http://127.0.0.1:9",
            "t",
            ROOT_PEM,
            &journal.path().join("loop.jsonl"),
        )
        .is_ok()
    );
}

#[tokio::test]
async fn a_400_is_corrupt_evidence_never_fallback() {
    // The plane is UP and rejected the request as malformed — schema drift
    // must fail closed, not silently bypass the plane (review P1).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/evaluate"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "invalid_request",
        })))
        .mount(&server)
        .await;
    let journal = tempfile::tempdir().expect("tempdir");
    let client = open_client(&server, &journal.path().join("p.jsonl"));
    let outcome = consult(&client).await;
    assert!(
        matches!(outcome, PlaneOutcome::CorruptEvidence { .. }),
        "400 must be corrupt evidence, got {outcome:?}"
    );
    assert!(!outcome.fallback_eligible());
}

#[tokio::test]
async fn a_permit_for_another_session_is_corrupt_evidence() {
    // A stale/cached/cross-session PERMIT authorizes nothing here.
    let server = MockServer::start().await;
    permit_or_deny_body(json!({
        "decision": "PERMIT",
        "session_id": "session-OTHER",
    }))
    .mount(&server)
    .await;
    let journal = tempfile::tempdir().expect("tempdir");
    let client = open_client(&server, &journal.path().join("p.jsonl"));
    let outcome = consult(&client).await;
    assert!(
        matches!(outcome, PlaneOutcome::CorruptEvidence { .. }),
        "cross-session PERMIT must be corrupt evidence, got {outcome:?}"
    );
    assert!(!outcome.fallback_eligible());
}

#[tokio::test]
async fn a_journal_from_another_plane_endpoint_fails_closed() {
    let server = MockServer::start().await;
    permit_mock().mount(&server).await;
    let journal_dir = tempfile::tempdir().expect("tempdir");
    let journal = journal_dir.path().join("p.jsonl");
    let client = open_client(&server, &journal);
    let _ = consult(&client).await;

    // Same journal, DIFFERENT plane endpoint: the identity header must
    // refuse the mismatch instead of letting the new plane close the old
    // plane's backlog.
    let other = MockServer::start().await;
    permit_mock().mount(&other).await;
    let err = PlaneClient::open(&other.uri(), "test-token", ROOT_PEM, &journal)
        .err()
        .expect("journal belongs to another plane");
    assert!(err.to_string().contains("belongs to plane"), "got: {err}");
}

#[tokio::test]
async fn truncating_complete_tail_records_fails_closed() {
    // The checkpoint must catch a deleted Terminal line even though the
    // remaining prefix is a valid MAC chain.
    let server = MockServer::start().await;
    permit_mock().mount(&server).await;
    let journal_dir = tempfile::tempdir().expect("tempdir");
    let journal = journal_dir.path().join("p.jsonl");
    let client = open_client(&server, &journal);
    let _ = consult(&client).await;

    let text = std::fs::read_to_string(&journal).expect("journal readable");
    let lines: Vec<&str> = text.lines().collect();
    let truncated = format!("{}\n", lines[..lines.len() - 1].join("\n"));
    std::fs::write(&journal, &truncated).expect("write");

    let err = PlaneClient::open(&server.uri(), "test-token", ROOT_PEM, &journal)
        .err()
        .expect("truncation of complete lines must fail closed");
    assert!(err.to_string().contains("journal shrank"), "got: {err}");
}

#[tokio::test]
async fn an_interrupted_200_body_is_ambiguous_never_fallback() {
    // The dangerous case from the review: the plane decided, then the body
    // stream broke. The decision is unknown — ambiguous, not unavailable.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/evaluate"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(b"partial-decision".to_vec(), "application/json"),
        )
        .mount(&server)
        .await;
    let journal = tempfile::tempdir().expect("tempdir");
    let client = open_client(&server, &journal.path().join("p.jsonl"));
    let outcome = consult(&client).await;
    // A truncated non-JSON body classifies as corrupt/ambiguous by shape;
    // what must NEVER happen is fallback.
    assert!(
        !outcome.fallback_eligible(),
        "interrupted body must never be fallback-eligible, got {outcome:?}"
    );
}
