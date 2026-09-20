//! #502 Seam B7 — the opt-in governance ER mirror at the commit decision.
//!
//! Phase 1 semantics: an Execution Receipt (ER) is mirrored **only** when a
//! round's native receipt durably commits. Abandoned / cancelled turns mint no
//! ER — every cancellation path returns before `commit_round`, and the terminal
//! cancellation marker (`record_turn_cancellation`) deliberately does not
//! mirror. A runtime built without `with_governance` behaves exactly as
//! before: binary allow/deny, no mirror file touched.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ardur_fused_runtime::{CancelProbe, ErMirrorEmitter, load_persisted_chain};
use ardur_governance::{
    ErRoundFacts, ErSigningKey, GovernanceEmitter, SignedExecutionReceipt, Verdict, verify_er_chain,
};
use ardur_provider_runtime::{
    CompletionResponse, FinishReason, Provider, ProviderError, RateCard, Usage,
};
use ardur_runtime::{ChatRuntime, ProviderId, RuntimeError, ToolCall};
use ardur_session_journals::InMemorySessionJournal;
use async_trait::async_trait;
use parking_lot::Mutex;

mod support;
use support::{
    EchoProvider, deny_all_policy, request_for, runtime_builder, runtime_builder_with_policy,
    valid_token,
};

/// The verifier identity the mirror stamps (ER idString-safe).
const VERIFIER_ID: &str = "spiffe://ardur/verifier/fused-runtime";

/// A scripted provider: pops queued responses, then repeats the default.
/// Shares its call counter so a cancel probe can fire on round boundaries.
struct ScriptedProvider {
    responses: Mutex<VecDeque<CompletionResponse>>,
    default: CompletionResponse,
    calls: Arc<AtomicUsize>,
    rate_card: RateCard,
}

impl ScriptedProvider {
    fn new(responses: Vec<CompletionResponse>, default: CompletionResponse) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            default,
            calls: Arc::new(AtomicUsize::new(0)),
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }

    fn calls_handle(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.calls)
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(
        &self,
        _request: ardur_provider_runtime::CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = self.responses.lock().pop_front();
        Ok(next.unwrap_or_else(|| self.default.clone()))
    }

    fn id(&self) -> ProviderId {
        ProviderId("scripted".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

fn stop(text: &str) -> CompletionResponse {
    CompletionResponse {
        content: text.to_string(),
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
        cost: ardur_runtime::CostTuple::default(),
        raw_provider_response: None,
    }
}

fn tool_call(id: &str, name: &str) -> CompletionResponse {
    CompletionResponse {
        content: String::new(),
        finish_reason: FinishReason::ToolUse(vec![ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({}),
        }]),
        usage: Usage::default(),
        cost: ardur_runtime::CostTuple::default(),
        raw_provider_response: None,
    }
}

fn echo_registry() -> Arc<ardur_tool_registry::ToolRegistry> {
    let mut registry = ardur_tool_registry::ToolRegistry::new();
    registry
        .register(Box::new(ardur_tool_registry::EchoTool::new()))
        .expect("echo registers");
    Arc::new(registry)
}

/// A cancel probe that reports "caller present" until the provider has been
/// called `rounds` times, then "caller gone" forever — modelling a client that
/// disconnects partway through a multi-round tool loop.
fn gone_after(calls: Arc<AtomicUsize>, rounds: usize) -> CancelProbe {
    Arc::new(move || calls.load(Ordering::SeqCst) >= rounds)
}

/// A capturing emitter: counts the mirror calls it is handed (and would
/// forward to an inner emitter in compositions).
struct CountingEmitter {
    calls: AtomicUsize,
}

impl CountingEmitter {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
        })
    }

    fn mirror_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl GovernanceEmitter for CountingEmitter {
    fn mirror_committed_round(
        &self,
        _facts: &ErRoundFacts<'_>,
    ) -> Result<(), ardur_governance::GovernanceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// (tempdir, mirror path, receipt log path) — one scratch root per test.
fn scratch() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let root = support::tempdir().expect("tempdir");
    let mirror = root.path().join("governance/er-chain.jsonl");
    let receipts = root.path().join("receipts.jsonl");
    (root, mirror, receipts)
}

/// Open the file-backed emitter over the test receipt-key custody.
fn open_emitter(mirror: &std::path::Path) -> Arc<ErMirrorEmitter> {
    Arc::new(
        ErMirrorEmitter::open(mirror, &support::receipt_key(), VERIFIER_ID)
            .expect("emitter opens over the native receipt key custody"),
    )
}

/// The ER public key set for the runtime's (process-stable) receipt key.
fn er_jwks() -> ardur_receipt::Jwks {
    ErSigningKey::from_pkcs8_pem(&support::receipt_key().to_pkcs8_pem().unwrap())
        .unwrap()
        .jwks()
}

/// Read the mirror log's compact JWS lines (empty when the file does not exist).
fn mirror_lines(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|s| {
            s.lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Verify every mirror line and rebuild the signed chain (fails the test on
/// any signature or linkage error) — through the CHECKED rebuild path, so
/// claims always come from the verified JWS itself.
fn signed_chain(path: &std::path::Path) -> Vec<SignedExecutionReceipt> {
    ardur_governance::verify_er_log_lines(&mirror_lines(path), &er_jwks())
        .expect("mirror chain verifies")
}

// ---------------------------------------------------------------------------
// 1. Emitter ON: a committed turn writes a verifiable ER.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_committed_turn_mirrors_a_verifiable_er() {
    let (_root, mirror_path, receipt_log) = scratch();
    let runtime = runtime_builder(Arc::new(EchoProvider::new()))
        .receipt_log(&receipt_log)
        .with_governance(open_emitter(&mirror_path))
        .build()
        .expect("runtime builds");

    let result = runtime
        .submit(request_for(
            "hello",
            &valid_token(),
            ardur_runtime::SessionId::new(),
        ))
        .await
        .expect("turn commits");

    let signed = signed_chain(&mirror_path);
    assert_eq!(signed.len(), 1, "one ER per committed round");
    let claims = signed[0].receipt();
    assert_eq!(claims.step_id, result.receipt_id.0.to_string());
    assert_eq!(
        claims.tool,
        support::TOOL,
        "the ER mirrors the turn capability"
    );
    assert_eq!(
        claims.actor,
        support::HOLDER,
        "the actor is the verified cap-token subject"
    );
    assert_eq!(
        claims.grant_id.len(),
        36,
        "grant_id is the hyphenated cap-token UUID"
    );
    assert_ne!(
        claims.grant_id, claims.step_id,
        "the ER names the grant, not the native receipt"
    );
    assert_eq!(claims.verdict, Verdict::Compliant);
    assert!(claims.parent_receipt_hash.is_none(), "genesis ER");
    assert_eq!(claims.verifier_id, VERIFIER_ID);
    verify_er_chain(&signed, &er_jwks()).expect("mirror chain verifies");
}

// ---------------------------------------------------------------------------
// 2. Abandoned turn: cancelled at the commit gate, no ER anywhere.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_abandoned_turn_mints_no_er() {
    let (_root, mirror_path, receipt_log) = scratch();
    let capturing = CountingEmitter::new();
    let runtime = runtime_builder(Arc::new(EchoProvider::new()))
        .receipt_log(&receipt_log)
        .with_governance(capturing.clone())
        .build()
        .expect("runtime builds");

    let probe: CancelProbe = Arc::new(|| true);
    let result = runtime
        .submit_with_cancellation(
            request_for("abandoned", &valid_token(), ardur_runtime::SessionId::new()),
            Default::default(),
            probe,
            None,
        )
        .await;

    assert!(matches!(result, Err(RuntimeError::TurnCancelled)));
    assert_eq!(capturing.mirror_count(), 0, "no mirror call may happen");
    assert!(
        mirror_lines(&mirror_path).is_empty(),
        "no ER line may be written"
    );
}

// ---------------------------------------------------------------------------
// 3. Opt-out: no with_governance — binary allow/deny unchanged, no mirror.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn without_governance_the_runtime_is_unchanged() {
    let (_root, mirror_path, receipt_log) = scratch();
    let session = ardur_runtime::SessionId::new();
    let journal: Arc<dyn ardur_session_journals::SessionJournal> =
        Arc::new(InMemorySessionJournal::new(session));

    // The permissive path still allows. The runtime is dropped before the
    // second build: the settlement writer lease is owner-bound (E4.3), so a
    // second runtime over the same log must not race a live one.
    let ok = {
        let allowed = runtime_builder(Arc::new(EchoProvider::new()))
            .receipt_log(&receipt_log)
            .with_journal(Arc::clone(&journal))
            .build()
            .expect("runtime builds");
        allowed
            .submit(request_for("allowed", &valid_token(), session))
            .await
    };
    assert!(ok.is_ok(), "permissive path still allows: {ok:?}");
    drop(ok);

    // Same setup over the deny-all bundle: still a binary policy deny.
    let denied = runtime_builder_with_policy(Arc::new(EchoProvider::new()), deny_all_policy())
        .receipt_log(&receipt_log)
        .with_journal(journal)
        .build()
        .expect("runtime builds");
    let err = denied
        .submit(request_for("denied", &valid_token(), session))
        .await
        .expect_err("deny-all policy still denies");
    assert!(
        matches!(err, RuntimeError::PolicyDenied { .. }),
        "got {err:?}"
    );

    assert!(
        !mirror_path.exists(),
        "no emitter configured — no mirror file may be created"
    );
}

// ---------------------------------------------------------------------------
// 4. Multi-round turn: one ER per committed round, chained; restart resumes.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multi_round_turns_chain_and_restart_resumes_the_mirror() {
    let (_root, mirror_path, receipt_log) = scratch();
    let provider =
        ScriptedProvider::new(vec![tool_call("call-1", "echo"), stop("done")], stop("d"));
    let runtime = runtime_builder(Arc::new(provider))
        .receipt_log(&receipt_log)
        .with_tools(echo_registry())
        .with_governance(open_emitter(&mirror_path))
        .build()
        .expect("runtime builds");

    let first = runtime
        .submit(request_for(
            "two rounds",
            &valid_token(),
            ardur_runtime::SessionId::new(),
        ))
        .await
        .expect("turn commits");

    let signed = signed_chain(&mirror_path);
    assert_eq!(signed.len(), 2, "one ER per committed round");
    verify_er_chain(&signed, &er_jwks()).expect("mirror chain verifies");
    assert_eq!(
        signed[1].receipt().parent_receipt_hash.as_deref(),
        Some(signed[0].receipt_hash().as_str()),
        "the second ER chains onto the first ER's JWS hash"
    );
    assert_eq!(
        signed[1].receipt().step_id,
        first.receipt_id.0.to_string(),
        "the final ER mirrors the final committed native receipt"
    );

    // Restart: a fresh runtime + emitter over the same paths resumes the chain.
    drop(runtime);
    let runtime2 = runtime_builder(Arc::new(EchoProvider::new()))
        .receipt_log(&receipt_log)
        .with_governance(open_emitter(&mirror_path))
        .build()
        .expect("runtime rebuilds over the persisted chains");
    runtime2
        .submit(request_for(
            "after restart",
            &valid_token(),
            ardur_runtime::SessionId::new(),
        ))
        .await
        .expect("post-restart turn commits");

    let signed = signed_chain(&mirror_path);
    assert_eq!(signed.len(), 3, "restart appends, never re-genesis");
    assert!(
        signed[2].receipt().parent_receipt_hash.is_some(),
        "the post-restart ER chains onto the pre-restart tail"
    );
    verify_er_chain(&signed, &er_jwks()).expect("the resumed mirror chain still verifies");
}

// ---------------------------------------------------------------------------
// 7. Review-round guards: torn tail, concurrent-writer fork, gap poisoning,
//    and persistence-keyed side-effect classification.
// ---------------------------------------------------------------------------

#[test]
fn a_torn_tail_fails_the_open_instead_of_chaining_onto_it() {
    let (_root, mirror, _receipts) = scratch();
    // Simulate a crash mid-append: a written log whose last line lost its
    // terminating newline.
    let emitter = ErMirrorEmitter::open(&mirror, &support::receipt_key(), VERIFIER_ID)
        .expect("emitter opens");
    emitter
        .mirror_committed_round(&facts_for_step("step-torn"))
        .expect("one ER writes");
    let bytes = std::fs::read(&mirror).expect("log exists");
    assert!(bytes.ends_with(b"\n"));
    let torn: Vec<u8> = bytes[..bytes.len() - 1].to_vec();
    std::fs::write(&mirror, &torn).expect("strip the newline");
    let err = ErMirrorEmitter::open(&mirror, &support::receipt_key(), VERIFIER_ID)
        .err()
        .expect("a torn tail must fail the open");
    match err {
        ardur_governance::GovernanceError::Io(m) => {
            assert!(
                m.contains("torn"),
                "expected the torn-tail diagnostic, got: {m}"
            );
        }
        other => panic!("expected Io, got {other:?}"),
    }
}

#[test]
fn a_foreign_append_between_ours_fails_closed_instead_of_forking() {
    let (_root, mirror, _receipts) = scratch();
    let emitter = ErMirrorEmitter::open(&mirror, &support::receipt_key(), VERIFIER_ID)
        .expect("emitter opens");

    // A foreign writer appends garbage after our (empty) state.
    std::fs::write(&mirror, b"foreign-writer-line\n").expect("foreign append");

    let err = match emitter.mirror_committed_round(&facts_for_step("step-x")) {
        Ok(()) => panic!("a changed log must fail the append"),
        Err(err) => err,
    };
    match err {
        ardur_governance::GovernanceError::Io(m) => {
            assert!(
                m.contains("changed under us"),
                "expected the fork-guard diagnostic, got: {m}"
            );
        }
        other => panic!("expected Io, got {other:?}"),
    }
}

#[test]
fn a_failed_append_poisons_the_emitter_so_gaps_stay_observable() {
    let (_root, mirror, _receipts) = scratch();
    let emitter = ErMirrorEmitter::open(&mirror, &support::receipt_key(), VERIFIER_ID)
        .expect("emitter opens");

    // Force a projection failure with an idString-invalid trace id.
    let bad = facts_for_trace("short");
    assert!(emitter.mirror_committed_round(&bad).is_err());

    // A later, otherwise-valid round must also fail: minting it would hide
    // the skipped round behind a continuous-looking chain.
    let err = match emitter.mirror_committed_round(&facts_for_step("step-y")) {
        Ok(()) => panic!("a poisoned emitter must refuse"),
        Err(err) => err,
    };
    match err {
        ardur_governance::GovernanceError::Io(m) => {
            assert!(
                m.contains("poisoned"),
                "expected the poison diagnostic, got: {m}"
            );
        }
        other => panic!("expected Io, got {other:?}"),
    }
    // And nothing was written.
    assert!(mirror_lines(&mirror).is_empty());
}

#[tokio::test]
async fn side_effect_class_tracks_persistence_not_tool_count() {
    let (_root, mirror, receipt_log) = scratch();
    let session = ardur_runtime::SessionId::new();

    // Journaling runtime, NO tool calls: still InternalWrite (transcript).
    let journaled = runtime_builder(Arc::new(EchoProvider::new()))
        .receipt_log(&receipt_log)
        .with_journal(Arc::new(InMemorySessionJournal::new(session))
            as Arc<dyn ardur_session_journals::SessionJournal>)
        .with_governance(open_emitter(&mirror))
        .build()
        .expect("runtime builds");
    journaled
        .submit(request_for("plain", &valid_token(), session))
        .await
        .expect("commits");
    let chain = signed_chain(&mirror);
    assert_eq!(
        chain[0].receipt().side_effect_class,
        ardur_governance::SideEffectClass::InternalWrite,
        "a journaled no-tool round persists transcript state"
    );
    drop(journaled);

    // Journal-less runtime, tool round: None (nothing durable changed).
    let plain = runtime_builder(Arc::new(ScriptedProvider::new(
        vec![tool_call("c1", "echo"), stop("done")],
        stop("d"),
    )))
    .receipt_log(&receipt_log)
    .with_tools(echo_registry())
    .with_governance(open_emitter(&mirror))
    .build()
    .expect("runtime builds");
    plain
        .submit(request_for(
            "tools",
            &valid_token(),
            ardur_runtime::SessionId::new(),
        ))
        .await
        .expect("commits");
    let chain = signed_chain(&mirror);
    assert_eq!(
        chain[1].receipt().side_effect_class,
        ardur_governance::SideEffectClass::None,
        "a journal-less tool round changed no durable state"
    );
}

/// Build a minimal fact bundle for direct emitter tests (valid trace/step).
fn facts_for_step(step: &'static str) -> ardur_governance::ErRoundFacts<'static> {
    facts_for_trace_and_step("trace:0123456789abcdef", step)
}

fn facts_for_trace(trace: &'static str) -> ardur_governance::ErRoundFacts<'static> {
    facts_for_trace_and_step(trace, "step:0123456789abcdef")
}

fn facts_for_trace_and_step(
    trace: &'static str,
    step: &'static str,
) -> ardur_governance::ErRoundFacts<'static> {
    ardur_governance::ErRoundFacts {
        claims: Box::leak(Box::new(verified_claims())),
        trace_id: trace,
        step_id: step,
        timestamp_millis: 1_750_000_000_000,
        tool: "chat.submit",
        provider: "echo",
        tool_calls: &[],
        persisted_transcript: false,
    }
}

/// Mint + verify a real cap-token, returning the verified claims (the same
/// substrate the runtime's stage 1 produces).
fn verified_claims() -> ardur_cap_token::VerifiedClaims {
    use ardur_cap_token::{
        BiscuitCapTokenIssuer, BiscuitCapTokenVerifier, CapScope, CapTokenIssuer, CapTokenVerifier,
        HolderId, KeyPair, RequiredCaveats,
    };
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let token = issuer
        .issue(
            HolderId(support::HOLDER.to_string()),
            CapScope {
                audience: support::AUDIENCE.to_string(),
                expires_unix: 4_000_000_000,
                budget_remaining: 1_000,
                tool_allowlist: vec![support::TOOL.to_string()],
            },
        )
        .expect("token issues");
    let verifier = BiscuitCapTokenVerifier::new(ardur_cap_token::HashSetDenyList::new());
    verifier
        .verify(
            &token,
            &issuer.public_key(),
            &RequiredCaveats {
                now_unix: 1_750_000_000,
                audience: support::AUDIENCE.to_string(),
                tool: support::TOOL.to_string(),
                cost: 1,
            },
        )
        .expect("claims verify")
}

#[tokio::test]
async fn the_data_dir_convention_lands_and_a_corrupt_log_fails_closed() {
    let (root, _mirror, _receipts) = scratch();
    let data_dir = root.path().join("state");

    // Convention path.
    let emitter =
        ErMirrorEmitter::open_in_data_dir(&data_dir, &support::receipt_key(), VERIFIER_ID)
            .expect("emitter opens at <data_dir>/governance/er-chain.jsonl");
    assert_eq!(
        emitter.path(),
        data_dir.join("governance/er-chain.jsonl").as_path()
    );

    // A corrupt line in an existing log must fail the open, not re-genesis.
    let log = data_dir.join("governance/er-chain.jsonl");
    std::fs::write(&log, b"not-a-jws\n").expect("corrupt the mirror");
    let err = ErMirrorEmitter::open_in_data_dir(&data_dir, &support::receipt_key(), VERIFIER_ID)
        .err()
        .expect("a corrupt mirror log must fail the open");
    assert!(
        matches!(
            err,
            ardur_governance::GovernanceError::Verify(_)
                | ardur_governance::GovernanceError::BrokenChain { .. }
                | ardur_governance::GovernanceError::Io(_)
        ),
        "got {err:?}"
    );
    // And the corrupt bytes were not overwritten.
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "not-a-jws\n");
}

#[tokio::test]
async fn a_mid_loop_cancel_mints_no_er_for_the_cancellation_marker() {
    let (_root, mirror_path, receipt_log) = scratch();
    let provider = ScriptedProvider::new(
        vec![tool_call("call-1", "echo"), tool_call("call-2", "echo")],
        stop("never reached"),
    );
    let calls = provider.calls_handle();

    let runtime = runtime_builder(Arc::new(provider))
        .receipt_log(&receipt_log)
        .with_tools(echo_registry())
        .with_governance(open_emitter(&mirror_path))
        .build()
        .expect("runtime builds");

    // The caller is gone from round 2 onward: round 1 commits (one ER), then
    // the post-provider cancel gate abandons the turn, appending the terminal
    // native cancellation marker — which must NOT mirror.
    let result = runtime
        .submit_with_cancellation(
            request_for(
                "mid-loop cancel",
                &valid_token(),
                ardur_runtime::SessionId::new(),
            ),
            Default::default(),
            gone_after(calls, 2),
            None,
        )
        .await;

    assert!(
        matches!(result, Err(RuntimeError::TurnCancelled)),
        "got {result:?}"
    );
    let native = load_persisted_chain(&receipt_log).expect("native chain loads");
    assert!(
        native.len() >= 2,
        "round 1 committed and the terminal cancellation marker appended, got {}",
        native.len()
    );
    assert_eq!(
        mirror_lines(&mirror_path).len(),
        1,
        "exactly the committed round mirrors; the cancellation marker mints no ER"
    );
    verify_er_chain(&signed_chain(&mirror_path), &er_jwks())
        .expect("the surviving mirror chain still verifies");
}
