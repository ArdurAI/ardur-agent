//! gh#533 guard tests: compaction preview/apply and background tasks are
//! admitted through the SAME Cedar + cost gates a chat turn passes, before
//! the provider is reached.
//!
//! Each guard is a contrast, not an independent claim: the positive control
//! (authorized + funded) really reaches the provider, and the negative cases
//! (missing/forbid policy, exhausted budget, expired/revoked token) deny with
//! the intended typed reason while the provider counter stays at zero.

mod support;

use std::sync::Arc;

use ardur_cedar_policy::{CedarPolicyBundle, PolicyBundle, PolicySource};
use ardur_cost_gate::CostEnvelope;
use ardur_runtime::{CapTokenRef, ChatMessage, RuntimeError, SessionId};
use ardur_session_journals::{FileSessionJournal, SessionJournal};

use support::{HOLDER, mint_token_as, permissive_policy};

fn compact_token() -> String {
    mint_token_as(HOLDER, support::AUDIENCE, &["context.compact"])
}

fn task_token() -> String {
    mint_token_as(HOLDER, support::AUDIENCE, &["task.background"])
}

/// A Cedar bundle that permits NOTHING (a single non-matching permit, no
/// forbid): the implicit-deny posture of a missing policy file.
fn missing_policy() -> CedarPolicyBundle {
    CedarPolicyBundle::load(PolicySource::Embedded(
        r#"permit(principal == User::"nobody", action, resource);"#.to_string(),
    ))
    .expect("the missing-policy stand-in compiles")
}

/// A Cedar bundle with a matching permit AND a matching forbid — an explicit
/// operator denial, distinct from a merely-absent policy.
fn forbid_policy() -> CedarPolicyBundle {
    CedarPolicyBundle::load(PolicySource::Embedded(format!(
        "permit(principal, action, resource);\n\
             forbid(principal == User::\"{HOLDER}\", action, resource);"
    )))
    .expect("the forbid policy compiles")
}

fn history() -> Vec<ChatMessage> {
    vec![
        ChatMessage::user("please refactor the auth module"),
        ChatMessage::assistant("done, see auth.rs"),
    ]
}

async fn build(
    policy: CedarPolicyBundle,
    budget_cents: u64,
    provider: Arc<support::EchoProvider>,
) -> ardur_fused_runtime::FusedRuntime {
    let session_id = SessionId::new();
    let journal = Arc::new(
        FileSessionJournal::new(support::tempdir().expect("journal dir").path(), session_id)
            .expect("journal opens"),
    );
    // Only the cents axis gates, at a one-cent envelope: a zero budget can
    // never admit even the smallest projected send.
    let envelope = CostEnvelope {
        tokens_in_max: 0,
        tokens_out_max: 0,
        cents_max: 1,
        wall_ms_max: 0,
        attention_score_max: 0,
    };
    support::runtime_builder_with_policy(provider, policy)
        .with_journal(journal)
        .projected_envelope(envelope)
        .provision_budget(
            support::gate_holder(),
            ardur_cost_gate::CostTuple {
                tokens_in: 0,
                tokens_out: 0,
                cents: budget_cents,
                wall_ms: 0,
                attention_score: 0,
            },
        )
        .build()
        .expect("runtime builds")
}

// ------------------------------------------------------------------
// Positive controls: authorized + funded really reaches the provider.
// ------------------------------------------------------------------

#[tokio::test]
async fn authorized_funded_compact_and_preview_reach_the_provider() {
    let provider = Arc::new(support::EchoProvider::new());
    let runtime = build(permissive_policy(), 1_000, provider.clone()).await;
    let cap_token = CapTokenRef(compact_token());

    let outcome = runtime
        .compact(
            SessionId::new(),
            &cap_token,
            "context.compact",
            &history(),
            None,
        )
        .await
        .expect("an authorized, funded compact succeeds");
    assert!(!outcome.receipt_id.0.is_nil(), "a receipt was minted");

    let summary = runtime
        .preview_compact(
            SessionId::new(),
            &cap_token,
            "context.compact",
            &history(),
            None,
        )
        .await
        .expect("an authorized, funded preview succeeds");
    assert!(!summary.is_empty());

    assert_eq!(
        provider.call_count(),
        2,
        "both sends really reached the peer"
    );
}

#[tokio::test]
async fn authorized_funded_background_task_reaches_the_provider() {
    let provider = Arc::new(support::EchoProvider::new());
    let runtime = build(permissive_policy(), 1_000, provider.clone()).await;
    let cap_token = CapTokenRef(task_token());

    let outcome = runtime
        .run_background_task(
            SessionId::new(),
            &cap_token,
            "task.background",
            "do a thing",
        )
        .await
        .expect("an authorized, funded task succeeds");
    assert!(outcome.result.is_some());
    assert_eq!(provider.call_count(), 1, "the send really reached the peer");
}

// ------------------------------------------------------------------
// Negative contrasts: zero provider dispatches under denial.
// ------------------------------------------------------------------

/// Both a missing (implicit-deny) and an explicit forbid policy deny compact
/// AND preview with PolicyDenied, before the provider is reached.
#[tokio::test]
async fn missing_or_forbid_policy_denies_compact_and_preview_without_dispatch() {
    for policy in [missing_policy(), forbid_policy()] {
        let provider = Arc::new(support::EchoProvider::new());
        let runtime = build(policy, 1_000, provider.clone()).await;
        let cap_token = CapTokenRef(compact_token());
        let session_id = SessionId::new();

        let err = runtime
            .compact(session_id, &cap_token, "context.compact", &history(), None)
            .await
            .expect_err("compact must be policy-denied");
        assert!(
            matches!(err, RuntimeError::PolicyDenied { .. }),
            "expected PolicyDenied, got {err:?}"
        );

        let err = runtime
            .preview_compact(session_id, &cap_token, "context.compact", &history(), None)
            .await
            .expect_err("preview must be policy-denied");
        assert!(
            matches!(err, RuntimeError::PolicyDenied { .. }),
            "expected PolicyDenied, got {err:?}"
        );

        assert_eq!(
            provider.call_count(),
            0,
            "no peer request may leave under a denied policy"
        );
    }
}

/// A zero budget denies compact AND preview with CostCeilingExceeded, before
/// the provider is reached — the exact chat-parity failure mode from the
/// issue's executed contrast (`--budget-cents 0`).
#[tokio::test]
async fn exhausted_budget_denies_compact_and_preview_without_dispatch() {
    let provider = Arc::new(support::EchoProvider::new());
    let runtime = build(permissive_policy(), 0, provider.clone()).await;
    let cap_token = CapTokenRef(compact_token());
    let session_id = SessionId::new();

    let err = runtime
        .compact(session_id, &cap_token, "context.compact", &history(), None)
        .await
        .expect_err("compact must be cost-denied");
    assert!(
        matches!(err, RuntimeError::CostCeilingExceeded),
        "expected CostCeilingExceeded, got {err:?}"
    );

    let err = runtime
        .preview_compact(session_id, &cap_token, "context.compact", &history(), None)
        .await
        .expect_err("preview must be cost-denied");
    assert!(
        matches!(err, RuntimeError::CostCeilingExceeded),
        "expected CostCeilingExceeded, got {err:?}"
    );

    assert_eq!(
        provider.call_count(),
        0,
        "no peer request may leave under an exhausted budget"
    );
}

/// gh#533 guard 5: the adjacent background-task path is denied the same way —
/// missing policy and zero budget both refuse before dispatch.
#[tokio::test]
async fn missing_policy_or_exhausted_budget_denies_background_task_without_dispatch() {
    for (policy, budget, expected) in [
        (missing_policy(), 1_000u64, "policy"),
        (permissive_policy(), 0, "budget"),
    ] {
        let provider = Arc::new(support::EchoProvider::new());
        let runtime = build(policy, budget, provider.clone()).await;
        let cap_token = CapTokenRef(task_token());

        let err = runtime
            .run_background_task(
                SessionId::new(),
                &cap_token,
                "task.background",
                "do a thing",
            )
            .await
            .expect_err("the background task must be denied");

        match expected {
            "policy" => assert!(
                matches!(err, RuntimeError::PolicyDenied { .. }),
                "expected PolicyDenied, got {err:?}"
            ),
            "budget" => assert!(
                matches!(err, RuntimeError::CostCeilingExceeded),
                "expected CostCeilingExceeded, got {err:?}"
            ),
            _ => unreachable!(),
        }
        assert_eq!(
            provider.call_count(),
            0,
            "no peer request may leave for a denied background task"
        );
    }
}

/// gh#533 guard 2: an expired cap-token is still refused (CapTokenExpired)
/// and a revoked one still denied — the new admission does not weaken the
/// existing token checks, and neither falls back to a weaker offline path.
#[tokio::test]
async fn expired_or_revoked_tokens_are_still_refused_without_dispatch() {
    use ardur_cap_token::{CapScope, CapTokenIssuer, HolderId as CapHolderId};

    let provider = Arc::new(support::EchoProvider::new());
    let runtime = build(permissive_policy(), 1_000, provider.clone()).await;

    // An expired token: expired before the runtime's manual clock "now".
    let issuer = support::cap_issuer();
    let expired = issuer
        .issue(
            CapHolderId(HOLDER.to_string()),
            CapScope {
                audience: support::AUDIENCE.to_string(),
                expires_unix: support::NOW_UNIX - 1,
                budget_remaining: 1_000_000,
                tool_allowlist: vec!["context.compact".to_string()],
            },
        )
        .expect("issues")
        .to_base64()
        .expect("serializes");
    let err = runtime
        .compact(
            SessionId::new(),
            &CapTokenRef(expired),
            "context.compact",
            &history(),
            None,
        )
        .await
        .expect_err("an expired token must be refused");
    assert!(
        matches!(err, RuntimeError::CapTokenExpired),
        "expected CapTokenExpired, got {err:?}"
    );

    // A revoked token: valid on its face, but revoked into the shared deny
    // list before the call.
    let valid = mint_token_as(HOLDER, support::AUDIENCE, &["context.compact"]);
    runtime
        .revoke_cap_token(
            SessionId::new(),
            CapTokenRef(valid.clone()),
            "test revocation",
        )
        .await
        .expect("revokes");
    let err = runtime
        .compact(
            SessionId::new(),
            &CapTokenRef(valid),
            "context.compact",
            &history(),
            None,
        )
        .await
        .expect_err("a revoked token must be refused");
    assert!(
        matches!(err, RuntimeError::CapDenied { .. }),
        "expected CapDenied, got {err:?}"
    );

    assert_eq!(
        provider.call_count(),
        0,
        "no peer request may leave under a refused token"
    );
}

/// gh#533 guard 3/4: settlement truthfulness. A successful preview mints a
/// `context.compact.previewed.v1` receipt — the durable audit record of the
/// external send — while installing nothing; and a provider failure on a
/// compact refunds the reserved envelope so the holder's balance is whole
/// again (no stranded hold).
#[tokio::test]
async fn preview_mints_an_audit_receipt_and_provider_failure_refunds() {
    // --- Preview leaves a chained audit receipt but installs nothing. ---
    let dir = support::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(dir.path(), session_id).expect("journal opens"));
    let receipt_dir = support::tempdir().expect("receipt dir");
    let receipt_log = tempfile::NamedTempFile::new_in(receipt_dir.path()).expect("receipt log");
    let provider = Arc::new(support::EchoProvider::new());
    let runtime = support::runtime_builder_with_policy(provider, permissive_policy())
        .with_journal(journal.clone())
        .receipt_log(receipt_log.path())
        .build()
        .expect("runtime builds");
    let cap_token = CapTokenRef(compact_token());

    let summary = runtime
        .preview_compact(session_id, &cap_token, "context.compact", &history(), None)
        .await
        .expect("preview succeeds");
    assert!(!summary.is_empty());

    let chain = ardur_fused_runtime::load_persisted_chain(receipt_log.path()).expect("chain loads");
    assert_eq!(
        chain.len(),
        1,
        "the external send leaves exactly one receipt"
    );
    assert_eq!(
        chain[0].body.verb.as_str(),
        "context.compact.previewed.v1",
        "preview is audited under its own verb"
    );
    ardur_fused_runtime::verify_persisted_chain(&chain).expect("the chain verifies");
    let entries = journal.replay(session_id).await.expect("journal replays");
    assert!(
        entries
            .iter()
            .all(|e| !matches!(e, ardur_session_journals::JournalEntry::Checkpoint { .. })),
        "preview installs no checkpoint"
    );

    // --- A provider failure on an applied compact refunds the envelope. ---
    let provider = Arc::new(support::ErroringProvider::new());
    let failure_session = SessionId::new();
    let failure_journal = Arc::new(
        FileSessionJournal::new(
            support::tempdir().expect("journal dir").path(),
            failure_session,
        )
        .expect("journal opens"),
    );
    let runtime = support::runtime_builder_with_policy(provider, permissive_policy())
        .with_journal(failure_journal)
        .projected_envelope(CostEnvelope {
            tokens_in_max: 0,
            tokens_out_max: 0,
            cents_max: 1,
            wall_ms_max: 0,
            attention_score_max: 0,
        })
        .provision_budget(
            support::gate_holder(),
            ardur_cost_gate::CostTuple {
                tokens_in: 0,
                tokens_out: 0,
                cents: 5,
                wall_ms: 0,
                attention_score: 0,
            },
        )
        .build()
        .expect("runtime builds");
    let before = runtime
        .remaining_budget(&support::gate_holder())
        .await
        .expect("holder provisioned");

    let err = runtime
        .compact(
            SessionId::new(),
            &CapTokenRef(compact_token()),
            "context.compact",
            &history(),
            None,
        )
        .await
        .expect_err("the failing provider surfaces as an error");
    assert!(
        matches!(err, RuntimeError::ProviderUnavailable),
        "expected the provider failure to surface, got {err:?}"
    );

    let after = runtime
        .remaining_budget(&support::gate_holder())
        .await
        .expect("holder provisioned");
    assert_eq!(
        before.cents, after.cents,
        "a provider failure refunds the reserved envelope — no stranded hold"
    );
}
