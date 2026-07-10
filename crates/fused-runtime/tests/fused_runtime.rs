//! Integration tests for the fused [`ChatRuntime`]: the happy path that lights
//! up every stage, plus one test per rejection seam (cap missing / forged /
//! expired / revoked, policy deny, cost ceiling, hook veto) and the
//! request-rewrite, observer, and receipt-chain behaviours.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ardur_cost_gate::{CostEnvelope, ManualClock};
use ardur_fused_runtime::{load_persisted_chain, verify_persisted_chain};
use ardur_lifecycle_hooks::{
    HookError, HookEvent, HookId, HookRegistry, LifecycleHook, LifecyclePhase, PostReceiptCtx,
    RecordingHook,
};
use ardur_memory::{HolderId as MemHolderId, InMemoryMemoryRuntime, MemoryRuntime, UnixTsMillis};
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, Provider, ProviderError, RateCard, Usage,
};
use ardur_receipt::Sha256Digest;
use ardur_runtime::{CapTokenRef, ChatRuntime, CostTuple, RuntimeError, SessionId};
use ardur_session_journals::{FileSessionJournal, JournalEntry, SessionJournal};
use async_trait::async_trait;
use parking_lot::Mutex;
use tokio::sync::Notify;

use support::{
    EchoProvider, HOLDER, NOW_MS, NOW_UNIX, RedactingHook, VetoHook, deny_all_policy, gate_holder,
    generous_budget, mint_token, request_for, runtime_builder, runtime_builder_with_policy,
    user_request, valid_token,
};

struct CapturingSignedJwsHook {
    seen: Arc<Mutex<Option<String>>>,
}

impl CapturingSignedJwsHook {
    fn new() -> Self {
        Self {
            seen: Arc::new(Mutex::new(None)),
        }
    }

    fn seen(&self) -> Option<String> {
        self.seen.lock().clone()
    }
}

#[async_trait]
impl LifecycleHook for CapturingSignedJwsHook {
    async fn on_post_receipt(&self, ctx: &PostReceiptCtx<'_>) -> Result<(), HookError> {
        *self.seen.lock() = Some(ctx.signed_receipt.jws_compact().to_string());
        Ok(())
    }

    fn hook_id(&self) -> HookId {
        HookId::new("capture-signed-jws")
    }
}

struct PausingProvider {
    started: Arc<Notify>,
    resume: Arc<Notify>,
    calls: AtomicUsize,
    rate_card: RateCard,
}

impl PausingProvider {
    fn new() -> Self {
        Self {
            started: Arc::new(Notify::new()),
            resume: Arc::new(Notify::new()),
            calls: AtomicUsize::new(0),
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }

    async fn wait_started(&self) {
        self.started.notified().await;
    }

    fn resume(&self) {
        self.resume.notify_one();
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for PausingProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        self.resume.notified().await;
        let content = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == ardur_runtime::Role::User)
            .map(|m| m.content.clone())
            .unwrap_or_default();
        Ok(CompletionResponse {
            content,
            finish_reason: FinishReason::Stop,
            usage: Usage {
                tokens_in: 1,
                tokens_out: 1,
                ..Default::default()
            },
            cost: CostTuple::default(),
            raw_provider_response: None,
        })
    }

    fn id(&self) -> ardur_runtime::ProviderId {
        ardur_runtime::ProviderId("pausing".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// The happy path drives every stage and persists to memory, the journal, and
/// the receipt log.
#[tokio::test]
async fn happy_path_runs_every_stage() {
    let provider = Arc::new(EchoProvider::new());
    let memory = Arc::new(InMemoryMemoryRuntime::new());
    let journal_dir = tempfile::tempdir().expect("temp dir");
    let session_id = SessionId::new();
    let journal =
        Arc::new(FileSessionJournal::new(journal_dir.path(), session_id).expect("journal opens"));
    let receipt_log = tempfile::NamedTempFile::new().expect("receipt log");

    let runtime = runtime_builder(provider.clone())
        .with_memory(memory.clone())
        .with_journal(journal.clone())
        .receipt_log(receipt_log.path())
        .build()
        .expect("runtime builds");

    let outcome = runtime
        .submit(request_for("hello substrate", &valid_token(), session_id))
        .await
        .expect("the happy-path turn completes");

    // 5. provider was dispatched and echoed the prompt.
    assert_eq!(provider.call_count(), 1);
    assert_eq!(outcome.response.content, "hello substrate");

    // 9. memory recorded exactly one fact for the holder.
    let visible = memory.current_as_of(&MemHolderId(HOLDER.to_string()), UnixTsMillis(NOW_MS + 1));
    assert_eq!(visible.len(), 1, "the turn is recorded as one memory fact");

    // 10. the journal persisted the user + assistant messages, replayable.
    let replayed = journal.replay(session_id).await.expect("journal replays");
    assert_eq!(replayed.len(), 2);
    assert!(matches!(replayed[0], JournalEntry::UserMessage { .. }));
    assert!(matches!(replayed[1], JournalEntry::AssistantMessage { .. }));

    // 6. the receipt log holds one genesis receipt.
    let chain = load_persisted_chain(receipt_log.path()).expect("chain loads");
    assert_eq!(chain.len(), 1);
    assert!(
        chain[0].body.parent_hash.is_none(),
        "first receipt is genesis"
    );
    verify_persisted_chain(&chain).expect("the single-receipt chain verifies");
}

/// §11.14b: the fused-runtime mint records the serving provider's
/// [`name`](ardur_provider_runtime::Provider::name) on the receipt body, so the
/// persisted (and signed) receipt names its backend. `EchoProvider` answers to
/// `"echo"`, inheriting the trait's default `name()` from its `id()`.
#[tokio::test]
async fn mint_records_provider_on_receipt() {
    let provider = Arc::new(EchoProvider::new());
    let session_id = SessionId::new();
    let receipt_log = tempfile::NamedTempFile::new().expect("receipt log");

    let runtime = runtime_builder(provider.clone())
        .receipt_log(receipt_log.path())
        .build()
        .expect("runtime builds");

    runtime
        .submit(request_for("hello substrate", &valid_token(), session_id))
        .await
        .expect("the turn completes");

    let chain = load_persisted_chain(receipt_log.path()).expect("chain loads");
    assert_eq!(chain.len(), 1);
    assert_eq!(
        chain[0].body.provider.as_deref(),
        Some("echo"),
        "the minted receipt names the serving provider"
    );
}

/// Stage 1: an empty cap-token is rejected before any provider call.
#[tokio::test]
async fn missing_cap_token_is_rejected() {
    let provider = Arc::new(EchoProvider::new());
    let runtime = runtime_builder(provider.clone()).build().expect("builds");

    let err = runtime
        .submit(user_request("hi", ""))
        .await
        .expect_err("an empty cap-token must be rejected");

    assert!(matches!(err, RuntimeError::CapTokenMissing));
    assert_eq!(
        provider.call_count(),
        0,
        "provider not reached on cap failure"
    );
}

/// Stage 1: a forged / unparseable cap-token is denied.
#[tokio::test]
async fn forged_cap_token_is_denied() {
    let provider = Arc::new(EchoProvider::new());
    let runtime = runtime_builder(provider.clone()).build().expect("builds");

    let err = runtime
        .submit(user_request("hi", "not-a-real-biscuit"))
        .await
        .expect_err("a forged cap-token must be denied");

    assert!(matches!(err, RuntimeError::CapDenied { .. }));
    assert_eq!(provider.call_count(), 0);
}

/// Stage 1: a once-valid but now-expired cap-token surfaces as `CapTokenExpired`.
#[tokio::test]
async fn expired_cap_token_is_denied() {
    let provider = Arc::new(EchoProvider::new());
    let runtime = runtime_builder(provider.clone()).build().expect("builds");

    // Expiry one second before the runtime's fixed "now".
    let expired = mint_token(NOW_UNIX - 1, 1_000_000);
    let err = runtime
        .submit(user_request("hi", &expired))
        .await
        .expect_err("an expired cap-token must be denied");

    assert!(matches!(err, RuntimeError::CapTokenExpired));
    assert_eq!(provider.call_count(), 0);
}

/// Stage 1: a token revoked mid-session is denied on the next turn, and the
/// revoke fires `on_revoke`.
#[tokio::test]
async fn revoked_token_is_denied_next_turn() {
    let provider = Arc::new(EchoProvider::new());
    let recorder = Arc::new(RecordingHook::new("rec"));
    let mut registry = HookRegistry::new();
    registry.register(recorder.clone());

    let runtime = runtime_builder(provider.clone())
        .registry(Arc::new(registry))
        .build()
        .expect("builds");

    let token = valid_token();
    let session_id = SessionId::new();

    // First turn succeeds.
    runtime
        .submit(request_for("before", &token, session_id))
        .await
        .expect("the pre-revocation turn succeeds");

    // Revoke, then the same token is denied.
    runtime
        .revoke_cap_token(session_id, CapTokenRef(token.clone()), "compromised")
        .await
        .expect("revoke succeeds");

    let err = runtime
        .submit(request_for("after", &token, session_id))
        .await
        .expect_err("a revoked token must be denied");
    assert!(matches!(err, RuntimeError::CapDenied { .. }));
    assert_eq!(
        provider.call_count(),
        1,
        "only the pre-revocation turn dispatched"
    );

    // The revoke fired an `on_revoke` callback.
    assert!(
        recorder
            .events()
            .iter()
            .any(|e| matches!(e, HookEvent::OnRevoke { .. })),
        "on_revoke fired for the revoked token"
    );
}

/// Stage 9: if a token is revoked after turn admission but before memory.write
/// re-verification, the turn remains non-fatal but the memory-write denial is
/// surfaced through `on_error` instead of being silently swallowed.
#[tokio::test]
async fn memory_write_revocation_reverification_fires_error_hook() {
    let provider = Arc::new(PausingProvider::new());
    let memory = Arc::new(InMemoryMemoryRuntime::new());
    let recorder = Arc::new(RecordingHook::new("rec"));
    let mut registry = HookRegistry::new();
    registry.register(recorder.clone());

    let runtime = Arc::new(
        runtime_builder(provider.clone())
            .with_memory(memory.clone())
            .registry(Arc::new(registry))
            .build()
            .expect("runtime builds"),
    );
    let token = valid_token();
    let session_id = SessionId::new();

    let submit_runtime = Arc::clone(&runtime);
    let submit_token = token.clone();
    let submit = tokio::spawn(async move {
        submit_runtime
            .submit(request_for(
                "revoke before memory",
                &submit_token,
                session_id,
            ))
            .await
    });

    provider.wait_started().await;
    runtime
        .revoke_cap_token(
            session_id,
            CapTokenRef(token.clone()),
            "revoked before memory write",
        )
        .await
        .expect("revocation succeeds while the provider call is in flight");
    provider.resume();

    let outcome = submit
        .await
        .expect("submit task joins")
        .expect("memory-write denial remains non-fatal");
    assert_eq!(outcome.response.content, "revoke before memory");
    assert_eq!(provider.call_count(), 1);

    let visible = memory.current_as_of(&MemHolderId(HOLDER.to_string()), UnixTsMillis(NOW_MS + 1));
    assert!(
        visible.is_empty(),
        "revoked memory.write re-verification must not write a memory fact"
    );

    let events = recorder.events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            HookEvent::OnError {
                phase: LifecyclePhase::MemoryWrite,
                message,
                ..
            } if message.contains("cap-token revoked")
        )),
        "revoked memory.write re-verification should fire an on_error event; events: {events:?}"
    );
}

/// Non-streaming memory authorization samples the clock at the side-effect,
/// not at turn admission, so a token that expires during provider work cannot
/// authorize a late memory write.
#[tokio::test]
async fn memory_write_expiry_is_rechecked_after_provider_work() {
    let provider = Arc::new(PausingProvider::new());
    let memory = Arc::new(InMemoryMemoryRuntime::new());
    let recorder = Arc::new(RecordingHook::new("rec"));
    let mut registry = HookRegistry::new();
    registry.register(recorder.clone());
    let clock = Arc::new(ManualClock::new(NOW_MS));

    let runtime = Arc::new(
        runtime_builder(provider.clone())
            .clock(clock.clone())
            .with_memory(memory.clone())
            .registry(Arc::new(registry))
            .build()
            .expect("runtime builds"),
    );
    let token = mint_token(NOW_UNIX + 1, 1_000_000);
    let session_id = SessionId::new();

    let submit_runtime = Arc::clone(&runtime);
    let submit = tokio::spawn(async move {
        submit_runtime
            .submit(request_for("expire before memory", &token, session_id))
            .await
    });

    provider.wait_started().await;
    clock.advance(2_000);
    provider.resume();

    let outcome = submit
        .await
        .expect("submit task joins")
        .expect("memory-write expiry remains non-fatal");
    assert_eq!(outcome.response.content, "expire before memory");
    assert!(
        memory
            .current_as_of(
                &MemHolderId(HOLDER.to_string()),
                UnixTsMillis(NOW_MS + 2_001)
            )
            .is_empty(),
        "expired memory.write authorization must not write a fact"
    );
    let events = recorder.events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            HookEvent::OnError {
                phase: LifecyclePhase::MemoryWrite,
                message,
                ..
            } if message.contains("expired")
        )),
        "expired memory.write should fire an on_error event; events: {events:?}"
    );
}

/// Stage 2: a deny-all Cedar bundle blocks the turn with `PolicyDenied`.
#[tokio::test]
async fn policy_deny_blocks_turn() {
    let provider = Arc::new(EchoProvider::new());
    let runtime = runtime_builder_with_policy(provider.clone(), deny_all_policy())
        .build()
        .expect("builds");

    let err = runtime
        .submit(user_request("hi", &valid_token()))
        .await
        .expect_err("a deny-all policy must block the turn");

    assert!(matches!(err, RuntimeError::PolicyDenied { .. }));
    assert_eq!(
        provider.call_count(),
        0,
        "provider not reached on policy denial"
    );
}

/// Stage 3: a budget that cannot cover the envelope is `CostCeilingExceeded`,
/// before any provider call.
#[tokio::test]
async fn cost_ceiling_blocks_underfunded_turn() {
    let provider = Arc::new(EchoProvider::new());
    // Re-provision the holder with a balance far below the default envelope.
    let runtime = runtime_builder(provider.clone())
        .provision_budget(
            gate_holder(),
            ardur_cost_gate::CostTuple {
                tokens_in: 1,
                tokens_out: 1,
                cents: 1,
                wall_ms: 1,
                attention_score: 1,
            },
        )
        .build()
        .expect("builds");

    let err = runtime
        .submit(user_request("hi", &valid_token()))
        .await
        .expect_err("an underfunded turn must be refused");

    assert!(matches!(err, RuntimeError::CostCeilingExceeded));
    assert_eq!(provider.call_count(), 0);
}

/// Stage 4: a pre-submit veto blocks the turn *and* releases the reservation it
/// had already taken at stage 3 (the budget is restored in full).
#[tokio::test]
async fn pre_submit_veto_blocks_and_releases_reservation() {
    let provider = Arc::new(EchoProvider::new());
    let mut registry = HookRegistry::new();
    registry.register(Arc::new(VetoHook::new("policy.deny", "blocked")));

    let runtime = runtime_builder(provider.clone())
        .registry(Arc::new(registry))
        .build()
        .expect("builds");

    let err = runtime
        .submit(user_request("hi", &valid_token()))
        .await
        .expect_err("a vetoing hook must block the turn");

    match err {
        RuntimeError::VetoedByHook { hook_id, reason } => {
            assert_eq!(hook_id, "policy.deny");
            assert_eq!(reason, "blocked");
        }
        other => panic!("expected VetoedByHook, got {other:?}"),
    }
    assert_eq!(provider.call_count(), 0, "veto blocks before the provider");

    // The reservation taken at stage 3 was released — the budget is whole again.
    let remaining = runtime
        .remaining_budget(&gate_holder())
        .await
        .expect("holder is provisioned");
    assert_eq!(
        remaining,
        generous_budget(),
        "a vetoed turn strands no reservation"
    );
}

/// Stage 4 + 6: a pre-submit replace rewrites the request; the provider sees the
/// rewritten prompt and the receipt's payload digest covers the rewritten
/// response.
#[tokio::test]
async fn pre_submit_replace_rewrites_request_and_receipt() {
    let provider = Arc::new(EchoProvider::new());
    let mut registry = HookRegistry::new();
    registry.register(Arc::new(RedactingHook::new("redactor")));
    let receipt_log = tempfile::NamedTempFile::new().expect("receipt log");

    let runtime = runtime_builder(provider.clone())
        .registry(Arc::new(registry))
        .receipt_log(receipt_log.path())
        .build()
        .expect("builds");

    let outcome = runtime
        .submit(user_request("my password is SECRET", &valid_token()))
        .await
        .expect("the redacted turn completes");

    // The provider saw the redacted prompt, and the echo returns it.
    let seen = provider.last_request().expect("provider was called");
    assert!(seen.messages.iter().all(|m| !m.content.contains("SECRET")));
    assert_eq!(outcome.response.content, "my password is [REDACTED]");

    // The receipt's payload digest covers the redacted response, not the original.
    let chain = load_persisted_chain(receipt_log.path()).expect("chain loads");
    assert_eq!(chain.len(), 1);
    assert_eq!(
        chain[0].body.payload_digest,
        Sha256Digest::of(outcome.response.content.as_bytes()),
        "the receipt digests the post-redaction response"
    );
}

/// Stage 7: a post-receipt observer sees the turn, in order after pre-submit.
#[tokio::test]
async fn post_receipt_observer_runs_after_pre_submit() {
    let provider = Arc::new(EchoProvider::new());
    let recorder = Arc::new(RecordingHook::new("rec"));
    let mut registry = HookRegistry::new();
    registry.register(recorder.clone());

    let runtime = runtime_builder(provider.clone())
        .registry(Arc::new(registry))
        .build()
        .expect("builds");

    runtime
        .submit(user_request("observe me", &valid_token()))
        .await
        .expect("the observed turn completes");

    let events = recorder.events();
    assert!(
        matches!(events.first(), Some(HookEvent::OnPreSubmit { .. })),
        "pre-submit fired first"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, HookEvent::OnPostReceipt { .. })),
        "post-receipt fired after the receipt was minted"
    );
}

/// Stage 7: the post-receipt hook sees the exact signed JWS that the receipt
/// log persists and the next receipt chains onto.
#[tokio::test]
async fn post_receipt_observer_sees_persisted_signed_jws() {
    let provider = Arc::new(EchoProvider::new());
    let receipt_log = tempfile::NamedTempFile::new().expect("receipt log");
    let capture = Arc::new(CapturingSignedJwsHook::new());
    let mut registry = HookRegistry::new();
    registry.register(capture.clone());

    let runtime = runtime_builder(provider.clone())
        .registry(Arc::new(registry))
        .receipt_log(receipt_log.path())
        .build()
        .expect("builds");

    let outcome = runtime
        .submit(user_request("observe signed", &valid_token()))
        .await
        .expect("the observed turn completes");

    let persisted = std::fs::read_to_string(receipt_log.path())
        .expect("receipt log reads")
        .trim()
        .to_string();
    let observed = capture
        .seen()
        .expect("post-receipt hook captured signed receipt");

    assert_eq!(
        observed, persisted,
        "hook-visible signed receipt is the persisted chain element"
    );
    let chain = load_persisted_chain(receipt_log.path()).expect("chain loads");
    assert_eq!(chain[0].body.receipt_id, outcome.receipt_id.0);
}

/// Stage 6: receipts chain across turns — the second receipt's `parent_hash`
/// is the SHA-256 of the first receipt's JWS.
#[tokio::test]
async fn receipts_chain_across_turns() {
    let provider = Arc::new(EchoProvider::new());
    let receipt_log = tempfile::NamedTempFile::new().expect("receipt log");
    let runtime = runtime_builder(provider.clone())
        .receipt_log(receipt_log.path())
        .build()
        .expect("builds");

    let token = valid_token();
    for prompt in ["turn one", "turn two", "turn three"] {
        runtime
            .submit(user_request(prompt, &token))
            .await
            .expect("each turn completes");
    }

    let chain = load_persisted_chain(receipt_log.path()).expect("chain loads");
    assert_eq!(chain.len(), 3);
    assert!(chain[0].body.parent_hash.is_none());
    for i in 1..chain.len() {
        assert_eq!(
            chain[i].body.parent_hash,
            Some(Sha256Digest::of(chain[i - 1].jws_compact.as_bytes())),
            "receipt {i} chains onto the prior receipt's JWS"
        );
    }
    verify_persisted_chain(&chain).expect("the three-receipt chain verifies");
}

/// ARD-486/487/490: concurrent turns must serialize receipt parent selection and
/// durable commit, leaving exactly one genesis receipt and a verifiable chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_turns_serialize_receipt_chain() {
    let provider = Arc::new(EchoProvider::new());
    let receipt_log = tempfile::NamedTempFile::new().expect("receipt log");
    let runtime = Arc::new(
        runtime_builder(provider.clone())
            .projected_envelope(CostEnvelope {
                cents_max: 1,
                ..Default::default()
            })
            .receipt_log(receipt_log.path())
            .build()
            .expect("builds"),
    );
    let token = valid_token();

    let mut joins = Vec::new();
    for i in 0..16 {
        let runtime = runtime.clone();
        let token = token.clone();
        joins.push(tokio::spawn(async move {
            runtime
                .submit(user_request(&format!("concurrent turn {i}"), &token))
                .await
        }));
    }
    for join in joins {
        join.await
            .expect("submit task joins")
            .expect("concurrent turn completes");
    }

    let chain = load_persisted_chain(receipt_log.path()).expect("chain loads");
    assert_eq!(chain.len(), 16);
    assert_eq!(
        chain
            .iter()
            .filter(|receipt| receipt.body.parent_hash.is_none())
            .count(),
        1,
        "only the first concurrent receipt may be a genesis receipt"
    );
    verify_persisted_chain(&chain).expect("the concurrent receipt chain verifies");
}

/// ARD-489: cost-gate finalization must happen before the receipt/journal commit
/// boundary. If a reservation expires during provider execution, the turn fails
/// without leaving a durable receipt or journal entry.
#[tokio::test]
async fn expired_reservation_does_not_commit_receipt_or_journal() {
    let provider = Arc::new(PausingProvider::new());
    let clock = Arc::new(ManualClock::new(NOW_MS));
    let journal_dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal =
        Arc::new(FileSessionJournal::new(journal_dir.path(), session_id).expect("journal opens"));
    let receipt_log = tempfile::NamedTempFile::new().expect("receipt log");
    let runtime = Arc::new(
        runtime_builder(provider.clone())
            .clock(clock.clone())
            .with_journal(journal.clone())
            .receipt_log(receipt_log.path())
            .build()
            .expect("runtime builds"),
    );
    let token = valid_token();

    let submit = {
        let runtime = runtime.clone();
        tokio::spawn(async move {
            runtime
                .submit(request_for("expires in provider", &token, session_id))
                .await
        })
    };
    provider.wait_started().await;
    clock.advance(31_000);
    provider.resume();

    let err = submit
        .await
        .expect("submit task joins")
        .expect_err("expired reservation fails before durable commit");
    assert!(matches!(err, RuntimeError::CostCeilingExceeded));
    assert!(
        load_persisted_chain(receipt_log.path())
            .expect("chain loads")
            .is_empty(),
        "no receipt should be durable after finalize rejects the turn"
    );
    assert!(
        journal
            .replay(session_id)
            .await
            .expect("journal replays")
            .is_empty(),
        "no journal entry should be durable after finalize rejects the turn"
    );
}

/// Stage 5: a provider failure releases the reservation and surfaces a runtime
/// error — the budget is not stranded.
#[tokio::test]
async fn provider_failure_releases_reservation() {
    let provider = Arc::new(support::ErroringProvider::new());
    let runtime = runtime_builder(provider.clone()).build().expect("builds");

    let err = runtime
        .submit(user_request("hi", &valid_token()))
        .await
        .expect_err("a failing provider surfaces an error");
    assert!(matches!(err, RuntimeError::ProviderUnavailable));
    assert_eq!(provider.call_count(), 1, "the provider was reached");

    let remaining = runtime
        .remaining_budget(&gate_holder())
        .await
        .expect("holder is provisioned");
    assert_eq!(
        remaining,
        generous_budget(),
        "a failed provider call strands no reservation"
    );
}
