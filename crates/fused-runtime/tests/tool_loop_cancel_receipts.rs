//! #422 — a turn cancelled mid tool-loop must not leave orphan receipts.
//!
//! #359 added the commit gate: a caller who goes away before the receipt is
//! minted is not billed. But the receipt log is append-only and the tool loop
//! mints one receipt per iteration, so a cancel arriving during iteration 2
//! cannot un-mint iteration 1's already-persisted receipt. A 504 after a
//! multi-round tool loop could therefore still leave durable receipts and
//! billing from earlier rounds, with no receipt recording that the turn ended
//! without settling.
//!
//! These tests pin the intended behaviour: whatever intermediate receipts a
//! cancelled tool loop leaves in the chain, the chain must also carry a
//! terminal record that the turn was cancelled, and the chain must stay
//! verifiable (parent_hash intact).

mod support;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ardur_fused_runtime::{load_persisted_chain, verify_persisted_chain};
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, Provider, ProviderError, RateCard, Usage,
};
use ardur_runtime::{CostTuple, ProviderId, RuntimeError, ToolCall};
use ardur_tool_registry::{EchoTool, ToolRegistry};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::json;

use support::{request_for, runtime_builder, valid_token};

/// A provider that returns a scripted queue of responses, one per call.
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

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// The shared call counter, so a cancel probe can fire on round boundaries.
    fn calls_handle(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.calls)
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn complete(
        &self,
        _request: CompletionRequest,
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

fn tool_call(id: &str, name: &str, args: serde_json::Value) -> CompletionResponse {
    CompletionResponse {
        content: String::new(),
        finish_reason: FinishReason::ToolUse(vec![ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args,
        }]),
        usage: Usage::default(),
        cost: CostTuple::default(),
        raw_provider_response: None,
    }
}

fn stop(text: &str) -> CompletionResponse {
    CompletionResponse {
        content: text.to_string(),
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
        cost: CostTuple::default(),
        raw_provider_response: None,
    }
}

fn echo_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(EchoTool::new()))
        .expect("echo registers");
    registry
}

/// A cancel probe that reports "caller present" until the provider has been
/// called `rounds` times, then "caller gone" forever after — modelling a client
/// that disconnects partway through a multi-round tool loop.
///
/// Keyed on the provider's call count rather than a probe-consultation count so
/// the test does not depend on how many times the loop happens to consult the
/// probe per round (it currently consults it four times: after the provider
/// round, before and after each tool invocation, and inside the commit lock).
fn probe_gone_after_rounds(
    calls: Arc<AtomicUsize>,
    rounds: usize,
) -> ardur_fused_runtime::CancelProbe {
    Arc::new(move || calls.load(Ordering::SeqCst) > rounds)
}

/// The #422 scenario: the loop completes round 1 (minting and persisting a
/// receipt) and the caller disconnects during round 2.
///
/// The turn must not settle, and the receipt chain must not be left with a
/// silent orphan: either no intermediate receipt survives, or a terminal
/// cancellation receipt records that the turn ended without settling. A chain
/// whose last entry is an intermediate tool round — implying a completed turn
/// that never happened — is the bug.
#[tokio::test]
async fn a_cancel_during_a_later_round_does_not_leave_a_silent_orphan_receipt() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log = root.path().join("receipts.jsonl");
    let session_id = ardur_runtime::SessionId::new();

    // Round 1 asks for a tool, round 2 asks again; the caller vanishes before
    // round 2's commit.
    let provider = Arc::new(ScriptedProvider::new(
        vec![
            tool_call("call_1", "echo", json!({ "msg": "round one" })),
            tool_call("call_2", "echo", json!({ "msg": "round two" })),
            stop("never reached"),
        ],
        stop("default"),
    ));

    let runtime = runtime_builder(provider.clone())
        .with_tools(echo_registry().into())
        .receipt_log(&receipt_log)
        .build()
        .expect("runtime builds");

    // The caller is present for round 1 (which mints and persists a receipt)
    // and gone from the moment round 2's provider call returns.
    let probe = probe_gone_after_rounds(provider.calls_handle(), 1);

    let result = runtime
        .submit_with_cancellation(
            request_for("multi-round turn", &valid_token(), session_id),
            Default::default(),
            probe,
            None,
        )
        .await;

    assert!(
        matches!(result, Err(RuntimeError::TurnCancelled)),
        "a turn abandoned mid-loop must not settle, got: {result:?}"
    );
    assert!(
        provider.call_count() >= 2,
        "the loop must actually have run more than one round for this to be \
         the #422 scenario (provider calls: {})",
        provider.call_count()
    );

    let chain = load_persisted_chain(&receipt_log).expect("chain load succeeds");

    // Whatever survives must be a verifiable chain — cancellation must never
    // corrupt parent_hash linkage.
    verify_persisted_chain(&chain).expect("the persisted chain still verifies after a cancel");

    if !chain.is_empty() {
        let last = chain.last().expect("non-empty");
        assert!(
            last.body.verb.as_str().contains("cancel"),
            "a chain left by a cancelled tool loop must end with a terminal \
             cancellation record, not an intermediate tool round — otherwise \
             the chain claims a turn completed that the caller never saw. \
             Chain verbs: {:?}",
            chain
                .iter()
                .map(|r| r.body.verb.as_str().to_string())
                .collect::<Vec<_>>()
        );
    }
}

/// A turn cancelled before ANY round commits stays exactly as #359 left it:
/// no receipts at all. This guards against a fix for #422 over-correcting into
/// minting cancellation receipts for turns that never committed anything.
#[tokio::test]
async fn a_cancel_before_the_first_commit_still_mints_nothing() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log = root.path().join("receipts.jsonl");
    let session_id = ardur_runtime::SessionId::new();

    let provider = Arc::new(ScriptedProvider::new(
        vec![tool_call("call_1", "echo", json!({ "msg": "one" }))],
        stop("default"),
    ));

    let runtime = runtime_builder(provider)
        .with_tools(echo_registry().into())
        .receipt_log(&receipt_log)
        .build()
        .expect("runtime builds");

    let probe: ardur_fused_runtime::CancelProbe = Arc::new(|| true);

    let result = runtime
        .submit_with_cancellation(
            request_for("abandoned immediately", &valid_token(), session_id),
            Default::default(),
            probe,
            None,
        )
        .await;

    assert!(
        matches!(result, Err(RuntimeError::TurnCancelled)),
        "got: {result:?}"
    );
    let chain = load_persisted_chain(&receipt_log).expect("chain load succeeds");
    assert!(
        chain.is_empty(),
        "a turn cancelled before any commit mints nothing (#359), got {} receipts",
        chain.len()
    );
}

/// An uncancelled multi-round loop is unaffected: it settles normally and the
/// chain ends with the final answer's receipt, not a cancellation record.
#[tokio::test]
async fn an_uncancelled_multi_round_loop_settles_normally() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log = root.path().join("receipts.jsonl");
    let session_id = ardur_runtime::SessionId::new();

    let provider = Arc::new(ScriptedProvider::new(
        vec![
            tool_call("call_1", "echo", json!({ "msg": "one" })),
            tool_call("call_2", "echo", json!({ "msg": "two" })),
            stop("settled"),
        ],
        stop("default"),
    ));

    let runtime = runtime_builder(provider.clone())
        .with_tools(echo_registry().into())
        .receipt_log(&receipt_log)
        .build()
        .expect("runtime builds");

    let probe: ardur_fused_runtime::CancelProbe = Arc::new(|| false);

    let result = runtime
        .submit_with_cancellation(
            request_for("multi-round settled", &valid_token(), session_id),
            Default::default(),
            probe,
            None,
        )
        .await
        .expect("an uncancelled loop settles");

    assert_eq!(result.response.content, "settled");
    assert_eq!(provider.call_count(), 3, "two tool rounds then the answer");

    let chain = load_persisted_chain(&receipt_log).expect("chain load succeeds");
    verify_persisted_chain(&chain).expect("chain verifies");
    assert_eq!(chain.len(), 3, "one receipt per provider round");
    assert!(
        !chain
            .last()
            .expect("non-empty")
            .body
            .verb
            .as_str()
            .contains("cancel"),
        "a settled turn must not be recorded as cancelled"
    );
}

/// Review follow-up (P1): the cancellation marker must carry ZERO cost.
///
/// Chain aggregators (`AppState::receipt_stats`,
/// `ardur_admin_ui::costs::aggregate_receipts`) sum `cost` across every receipt
/// without inspecting the verb, so restating the cumulative total on a terminal
/// marker would double-count a cancelled turn's spend.
#[tokio::test]
async fn a_cancellation_marker_carries_zero_cost() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log = root.path().join("receipts.jsonl");
    let session_id = ardur_runtime::SessionId::new();

    let provider = Arc::new(ScriptedProvider::new(
        vec![
            tool_call("call_1", "echo", json!({ "msg": "one" })),
            tool_call("call_2", "echo", json!({ "msg": "two" })),
            stop("never reached"),
        ],
        stop("default"),
    ));

    let runtime = runtime_builder(provider.clone())
        .with_tools(echo_registry().into())
        .receipt_log(&receipt_log)
        .build()
        .expect("runtime builds");

    let probe = probe_gone_after_rounds(provider.calls_handle(), 1);
    let _ = runtime
        .submit_with_cancellation(
            request_for("cost check", &valid_token(), session_id),
            Default::default(),
            probe,
            None,
        )
        .await;

    let chain = load_persisted_chain(&receipt_log).expect("chain loads");
    let markers: Vec<_> = chain
        .iter()
        .filter(|r| r.body.verb.as_str().contains("cancel"))
        .collect();
    assert!(!markers.is_empty(), "expected a cancellation marker");
    for m in markers {
        assert_eq!(m.body.cost.cents, 0, "marker must not restate billed cents");
        assert_eq!(m.body.cost.tokens_in, 0);
        assert_eq!(m.body.cost.tokens_out, 0);
        assert!(
            m.body.tool_calls.is_empty(),
            "marker must not restate tool calls"
        );
    }
}

/// Guards the lock discipline the #422 cancellation path depends on.
///
/// `commit_lock` is a non-reentrant `AsyncMutex`. `record_turn_cancellation`
/// acquires it. Any call to it from inside the commit block — which holds
/// `commit_guard` — therefore parks the turn worker on a lock it already
/// holds, and because the runtime has a single worker that stalls every
/// subsequent turn, not just the cancelled one.
///
/// This is asserted structurally rather than behaviourally on purpose. The
/// dangerous branch is currently unreachable with `committed_rounds > 0`:
/// `begin_persist` only refuses from Cancelled, `request_cancel` only wins
/// against Live, and any committed round has already moved the handshake to
/// Committed, where `record_turn_cancellation` early-returns. A runtime test
/// would pass today whether or not the guard were released, and would go
/// vacuous the moment that coincidence changed — which is precisely when the
/// deadlock would appear. Pinning the source invariant fails loudly instead.
#[test]
fn recording_a_cancellation_never_happens_while_the_commit_guard_is_held() {
    let src = include_str!("../src/runtime.rs");
    let lines: Vec<&str> = src.lines().collect();

    // Locate the commit block: `let (signed, receipt) = {` ... matching `};`
    // at the same indentation.
    let open = lines
        .iter()
        .position(|l| l.trim_start().starts_with("let (signed, receipt) = {"))
        .expect("commit block should exist; if this function was renamed, re-point this guard");
    let indent = lines[open].len() - lines[open].trim_start().len();
    let closer = format!("{}}};", " ".repeat(indent));
    let close = lines[open + 1..]
        .iter()
        .position(|l| *l == closer)
        .map(|i| i + open + 1)
        .expect("commit block should close");

    // A `drop(commit_guard)` only releases the guard for the branch it sits in.
    // An early-return path that drops before returning does NOT make a sibling
    // branch safe, so track brace depth: a call is safe only when a drop at the
    // SAME depth precedes it.
    let mut depth: i32 = 0;
    let mut dropped_at_depth: Vec<i32> = Vec::new();
    let mut offenders = Vec::new();
    for (idx, line) in lines[open..=close].iter().enumerate() {
        let lineno = open + idx + 1;
        let code = line.split("//").next().unwrap_or("");

        if code.contains("drop(commit_guard)") {
            dropped_at_depth.push(depth);
        }
        if code.contains("record_turn_cancellation") && !dropped_at_depth.contains(&depth) {
            offenders.push(lineno);
        }

        let opens = code.matches('{').count() as i32;
        let closes = code.matches('}').count() as i32;
        depth += opens - closes;
        // Leaving a branch invalidates drops recorded inside it.
        if closes > opens {
            dropped_at_depth.retain(|d| *d <= depth);
        }
    }

    assert!(
        offenders.is_empty(),
        "record_turn_cancellation is called at line(s) {offenders:?} while the \
         commit guard is still held. That re-acquires the non-reentrant \
         commit_lock and deadlocks the single turn worker, stalling every \
         later turn. Release the guard first: `drop(commit_guard);`"
    );
}
