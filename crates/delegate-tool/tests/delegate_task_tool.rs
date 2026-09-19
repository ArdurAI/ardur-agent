//! End-to-end tests of the `delegate_task` `Tool` adapter: args parsing, the
//! happy path through the real `ardur-multi-agent` substrate, receipt
//! chaining, and the denial path when the caller's cap-token cannot back a
//! child turn.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ardur_cap_token::{
    BiscuitCapTokenIssuer, CapScope, CapTokenIssuer, HolderId, KeyPair, PublicKey,
};
use ardur_delegate_child::{ChildOutcome, ParentBudget};
use ardur_delegate_tool::DelegateTaskTool;
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, CostTuple, FinishReason, ModelId, Provider,
    ProviderError, ProviderId, RateCard, Usage,
};
use ardur_runtime::{CapTokenRef, SessionId};
use ardur_tool_registry::{InvocationId, Tool, ToolContext, ToolError};
use async_trait::async_trait;
use serde_json::json;

const AUDIENCE: &str = "ardur";
const EXPIRY_UNIX: u64 = 4_000_000_000; // ~2096, far past any test's runtime.

/// Simple echo mock for D1 tests: returns the last user message content as the
/// reply (mimics the old in-memory echo for compatibility of existing tests),
/// with fixed 100-cent cost.
struct EchoMock;

#[async_trait]
impl Provider for EchoMock {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let content = req
            .messages
            .last()
            .map(|m| m.content.clone())
            .unwrap_or_default();
        Ok(CompletionResponse {
            content,
            finish_reason: FinishReason::Stop,
            usage: Usage {
                tokens_in: 10,
                tokens_out: 5,
                cost_cents: Some(100),
            },
            cost: CostTuple {
                cents: 100,
                ..CostTuple::default()
            },
            raw_provider_response: None,
        })
    }

    fn id(&self) -> ProviderId {
        ProviderId("echo-mock".into())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        static CARD: std::sync::OnceLock<RateCard> = std::sync::OnceLock::new();
        CARD.get_or_init(|| RateCard {
            version_id: "echo-test-v1".into(),
            cents_per_1k_input: 0.0,
            cents_per_1k_output: 0.0,
            cents_per_request: 0.0,
        })
    }
}

fn echo_provider() -> Arc<dyn Provider + Send + Sync> {
    Arc::new(EchoMock)
}

/// Issue a parent cap-token granting `tools`, returning it (base64) alongside
/// the issuer root the tool must be constructed with.
fn parent_token(tools: &[&str], budget: u64) -> (String, PublicKey) {
    parent_token_with_expiry(tools, budget, EXPIRY_UNIX)
}

fn parent_token_with_expiry(tools: &[&str], budget: u64, expires_unix: u64) -> (String, PublicKey) {
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let root = issuer.public_key();
    let token = issuer
        .issue(
            HolderId("spiffe://ardur/agent/parent-test".to_string()),
            CapScope {
                audience: AUDIENCE.to_string(),
                expires_unix,
                budget_remaining: budget,
                tool_allowlist: tools.iter().map(|t| t.to_string()).collect(),
            },
        )
        .expect("issue parent token");
    (token.to_base64().expect("encode parent token"), root)
}

fn ctx_with(cap_token: String, invocation_id: InvocationId) -> ToolContext {
    ToolContext {
        cap_token: CapTokenRef(cap_token),
        session_id: SessionId::new(),
        invocation_id,
        cwd: PathBuf::from("."),
        env: HashMap::new(),
        cost_budget_cents: 1_000,
    }
}

#[test]
fn default_tool_id_remains_available_without_type_arguments() {
    let id: &'static str = DelegateTaskTool::ID;
    assert_eq!(id, "delegate_task");
}

#[tokio::test]
async fn delegate_task_completes_and_chains_receipt_to_invocation_id() {
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = DelegateTaskTool::with_provider(root, AUDIENCE, echo_provider());
    let invocation_id = InvocationId::new();
    let ctx = ctx_with(token, invocation_id);

    let output = tool
        .invoke(&ctx, json!({ "goal": "summarize the incident report" }))
        .await
        .expect("delegate_task should complete");

    assert_eq!(output.content["outcome"], "completed");
    // The echo child runtime returns the user content back as the reply.
    assert_eq!(output.content["response"], "summarize the incident report");
    assert_eq!(output.content["cents_used"], 100);

    // The termination receipt's parent anchor is exactly this call's
    // invocation id — the link an auditor walks from the fused runtime's
    // ToolCallReceipt to this sub-agent's termination receipt.
    assert_eq!(
        output.receipt_data["parent_receipt_id"],
        invocation_id.0.to_string()
    );
    assert_eq!(output.cost.cents, 100);
}

#[tokio::test]
async fn delegate_task_honors_max_cost_cents_override() {
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = DelegateTaskTool::with_provider(root, AUDIENCE, echo_provider());
    let ctx = ctx_with(token, InvocationId::new());

    let output = tool
        .invoke(
            &ctx,
            json!({ "goal": "draft a changelog entry", "max_cost_cents": 5 }),
        )
        .await
        .expect("delegate_task should complete (or exhaust on first over-bill)");

    // Truthful settlement (D1): the child provider billed 100; the declared
    // envelope of 5 was used for admission and caused overdrawn stop after the
    // round. Reported cost is actual, not the declared cap, and the terminal
    // label is the truthful budget_exhausted, not a collapsed "failed".
    assert_eq!(output.content["cents_used"], 100);
    assert_eq!(output.content["outcome"], "budget_exhausted");
}

#[tokio::test]
async fn delegate_task_denies_at_the_concurrency_ceiling() {
    // A zero ceiling makes admission deterministically fail on the very first
    // call, with no dependence on real-time scheduling — the blueprint's
    // default budget envelope names `max_concurrency = 3`; this proves the
    // gate itself is enforced (and fails fast, spawning nothing) rather than
    // racing real concurrent calls against wall-clock timing.
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = DelegateTaskTool::with_max_concurrency(root, AUDIENCE, 0, Some(echo_provider()));
    let ctx = ctx_with(token, InvocationId::new());

    let err = tool
        .invoke(&ctx, json!({ "goal": "should never spawn" }))
        .await
        .expect_err("a call at the concurrency ceiling must be denied");

    match err {
        ToolError::Denied { reason } => {
            assert!(reason.contains("concurrency ceiling (0)"), "got: {reason}");
        }
        other => panic!("expected Denied, got {other:?}"),
    }
}

#[tokio::test]
async fn delegate_task_releases_its_permit_after_completing() {
    // A ceiling of exactly one: the first call must succeed and, on
    // returning, must release its permit so a second, later call is not
    // permanently locked out.
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = DelegateTaskTool::with_max_concurrency(root, AUDIENCE, 1, Some(echo_provider()));

    let first = tool
        .invoke(
            &ctx_with(token.clone(), InvocationId::new()),
            json!({ "goal": "first" }),
        )
        .await
        .expect("first call should complete");
    assert_eq!(first.content["outcome"], "completed");

    let second = tool
        .invoke(
            &ctx_with(token, InvocationId::new()),
            json!({ "goal": "second" }),
        )
        .await
        .expect("second call should complete once the first released its permit");
    assert_eq!(second.content["outcome"], "completed");
}

#[tokio::test]
async fn delegate_task_rejects_empty_goal() {
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = DelegateTaskTool::with_provider(root, AUDIENCE, echo_provider());
    let ctx = ctx_with(token, InvocationId::new());

    let err = tool
        .invoke(&ctx, json!({ "goal": "" }))
        .await
        .expect_err("empty goal must be rejected");

    assert!(matches!(err, ToolError::InvalidArgs(_)));
}

#[tokio::test]
async fn delegate_task_rejects_missing_goal_field() {
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = DelegateTaskTool::with_provider(root, AUDIENCE, echo_provider());
    let ctx = ctx_with(token, InvocationId::new());

    let err = tool
        .invoke(&ctx, json!({ "task_name": "no goal here" }))
        .await
        .expect_err("missing goal must be rejected");

    assert!(matches!(err, ToolError::InvalidArgs(_)));
}

#[tokio::test]
async fn delegate_task_fails_when_parent_token_lacks_chat_submit() {
    // A parent token that never granted `chat.submit` in the first place:
    // attenuation only narrows, so the child cannot gain it either. The child
    // spawns, but its first (only) turn is denied at the real-wire boundary.
    let (token, root) = parent_token(&["some.other.tool"], 10_000);
    let tool = DelegateTaskTool::with_provider(root, AUDIENCE, echo_provider());
    let ctx = ctx_with(token, InvocationId::new());

    let err = tool
        .invoke(&ctx, json!({ "goal": "this should not run" }))
        .await
        .expect_err("a child without chat.submit must be denied");

    match err {
        ToolError::CapTokenDenied { reason } => {
            assert!(
                reason.contains("tool not in")
                    || reason.contains("denied")
                    || reason.contains("allowlist"),
                "expected a cap-token denial for missing tool, got: {reason}"
            );
        }
        other => panic!("expected CapTokenDenied, got {other:?}"),
    }
}

#[tokio::test]
async fn delegate_task_rejects_undecodable_cap_token() {
    let (_token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = DelegateTaskTool::with_provider(root, AUDIENCE, echo_provider());
    let ctx = ctx_with("not-a-real-cap-token".to_string(), InvocationId::new());

    let err = tool
        .invoke(&ctx, json!({ "goal": "should never spawn" }))
        .await
        .expect_err("a malformed cap-token must be denied before spawn");

    assert!(
        matches!(err, ToolError::CapTokenDenied { .. }),
        "malformed parent credential must be a typed token denial, got {err:?}"
    );
}

#[tokio::test]
async fn expired_parent_is_a_token_denial_not_an_execution_failure() {
    let (token, root) = parent_token_with_expiry(&["chat.submit"], 10_000, 1);
    let tool = DelegateTaskTool::with_provider(root, AUDIENCE, echo_provider());
    let err = tool
        .invoke(
            &ctx_with(token, InvocationId::new()),
            json!({"goal": "must not run"}),
        )
        .await
        .expect_err("expired credential must not authorize a child");
    assert!(
        matches!(err, ToolError::CapTokenDenied { .. }),
        "expired credential must stay a typed token denial, got {err:?}"
    );
}

#[test]
fn schema_requires_goal_and_advertises_the_delegate_capability() {
    // D1: schema test uses with_provider(echo) to avoid any from_env dep.
    let tool = DelegateTaskTool::with_provider(KeyPair::new().public(), AUDIENCE, echo_provider());
    assert_eq!(tool.id().as_str(), "delegate_task");

    let schema = tool.schema();
    let required = schema.input_schema["required"]
        .as_array()
        .expect("required array");
    assert!(required.iter().any(|v| v == "goal"));

    assert_eq!(tool.required_capabilities().len(), 1);
}

/// gh#361 half two: a parent token revoked through the shared deny list must
/// make `delegate_task` fail — the child's runtime consults the same list the
/// revoker writes to. Before this wiring, each delegation got a private empty
/// deny list, so a revocation was invisible to it.
#[tokio::test]
async fn revoked_parent_token_cannot_delegate() {
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    // Use SharedDenyList so that clone shares revocation state (the test's point).
    // Set provider for the internal new() call inside with_deny_list.
    // SAFETY: single-threaded test setup only.
    unsafe {
        std::env::set_var("ARDUR_PROVIDER", "prime");
    }
    let deny = ardur_fused_runtime::SharedDenyList::new();
    let tool = DelegateTaskTool::with_deny_list(root, AUDIENCE, deny.clone());

    // Revoke the caller's token through the shared handle (the runtime's
    // revoke_cap_token path in production).
    let parsed = ardur_cap_token::CapToken::from_base64(&token, &root).expect("parse");
    let _ = deny.revoke_token(&parsed); // SharedDenyList returns Result; ignore for test

    let ctx = ctx_with(token, InvocationId::new());
    let err = tool
        .invoke(&ctx, json!({ "goal": "should be denied" }))
        .await
        .expect_err("a revoked caller's delegation must be denied");
    assert!(
        matches!(err, ToolError::CapTokenDenied { .. }),
        "revoked parent must remain a typed token denial, got {err:?}"
    );
}

/// The shared list must not deny unrevoked tokens (no fail-closed overreach).
/// D1 fix: use explicit echo_provider() (always completes with "completed") so the
/// test is not sensitive to CI runner provider selection (prime/from_env can return
/// "failed" on some environments). The revoked test already covers denial with shared deny.
#[tokio::test]
async fn unrevoked_parent_token_delegates_with_shared_deny_list() {
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = DelegateTaskTool::with_provider(root, AUDIENCE, echo_provider());
    let ctx = ctx_with(token, InvocationId::new());
    let output = tool
        .invoke(&ctx, json!({ "goal": "still works" }))
        .await
        .expect("an unrevoked token must still delegate");
    assert_eq!(output.content["outcome"], "completed");
}

#[tokio::test]
async fn real_child_turn_completes() {
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = DelegateTaskTool::with_provider(root, AUDIENCE, echo_provider());
    let ctx = ctx_with(token, InvocationId::new());
    let output = tool
        .invoke(&ctx, json!({ "goal": "real child test" }))
        .await
        .expect("real supervised child should complete");
    assert_eq!(output.content["outcome"], "completed");
}

/// A provider whose round blocks until released, so tests can observe a live
/// in-flight child deterministically instead of racing wall-clock timing.
/// Release is a polled flag, NOT a Notify: `notify_waiters` only wakes
/// already-registered waiters, so a second round started after the notify
/// would hang forever.
struct GatedMock {
    started: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    cents: u64,
}

#[async_trait]
impl Provider for GatedMock {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.started.store(true, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(CompletionResponse {
            content: "gated reply".to_string(),
            finish_reason: FinishReason::Stop,
            usage: Usage {
                tokens_in: 10,
                tokens_out: 5,
                cost_cents: Some(self.cents),
            },
            cost: CostTuple {
                cents: self.cents,
                ..CostTuple::default()
            },
            raw_provider_response: None,
        })
    }

    fn id(&self) -> ProviderId {
        ProviderId("gated-mock".into())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        static CARD: std::sync::OnceLock<RateCard> = std::sync::OnceLock::new();
        CARD.get_or_init(|| RateCard {
            version_id: "gated-test-v1".into(),
            cents_per_1k_input: 0.0,
            cents_per_1k_output: 0.0,
            cents_per_request: 0.0,
        })
    }
}

fn gated_provider(started: &Arc<AtomicBool>, release: &Arc<AtomicBool>, cents: u64) -> Arc<dyn Provider + Send + Sync> {
    Arc::new(GatedMock {
        started: Arc::clone(started),
        release: Arc::clone(release),
        cents,
    })
}

async fn wait_for_start(flag: &AtomicBool) {
    for _ in 0..300 {
        if flag.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("provider round never started — fixture broken, not the code under test");
}

async fn wait_for_settlement(
    tool: &DelegateTaskTool,
) -> Vec<Result<ChildOutcome, ardur_delegate_child::ChildError>> {
    for _ in 0..300 {
        let drained = tool.drain_settlements();
        if !drained.is_empty() {
            return drained;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no retained settlement after waiter drop — paid work would be unaccounted");
}

#[tokio::test]
async fn dropped_future_not_detached() {
    // Poll the invocation until the provider round has STARTED, then cancel the
    // waiter (drop the invoke future and with it the oneshot receiver). The
    // retained driver must still run the worker to completion, and the paid
    // work's accounting must survive in the settlements buffer — a dropped
    // future is not termination, and undelivered cost is not zero cost.
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let provider = gated_provider(&started, &release, 25);
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = Arc::new(DelegateTaskTool::with_provider(root, AUDIENCE, provider));
    let ctx = ctx_with(token, InvocationId::new());

    let invoke_tool = Arc::clone(&tool);
    let waiter =
        tokio::spawn(async move { invoke_tool.invoke(&ctx, json!({ "goal": "drop me" })).await });
    wait_for_start(&started).await;
    waiter.abort();

    release.store(true, Ordering::SeqCst);
    let settled = wait_for_settlement(&tool).await;
    assert_eq!(
        settled.len(),
        1,
        "exactly one terminal outcome must be retained"
    );
    match &settled[0] {
        Ok(ChildOutcome::Completed { cost, .. }) => {
            assert_eq!(
                cost.cents, 25,
                "the billed round's real cost must be retained"
            );
        }
        other => panic!("expected a Completed retained outcome, got {other:?}"),
    }
}

#[tokio::test]
async fn permit_lifetime_vs_cancelled_waiter() {
    // Concurrency 1, live in-flight child: cancelling the caller's WAITER must
    // not release capacity while the provider worker still runs. A second call
    // is denied until real termination, then admitted.
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let provider = gated_provider(&started, &release, 25);
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = Arc::new(DelegateTaskTool::with_max_concurrency(
        root,
        AUDIENCE,
        1,
        Some(provider),
    ));

    let invoke_tool = Arc::clone(&tool);
    let ctx = ctx_with(token.clone(), InvocationId::new());
    let waiter = tokio::spawn(async move {
        invoke_tool
            .invoke(&ctx, json!({ "goal": "hold permit" }))
            .await
    });
    wait_for_start(&started).await;
    waiter.abort();

    // Worker is still gated (alive). Capacity must NOT be free.
    let denied = tool
        .invoke(
            &ctx_with(token.clone(), InvocationId::new()),
            json!({ "goal": "must be denied" }),
        )
        .await
        .expect_err("a cancelled waiter must not release the worker's permit");
    assert!(
        matches!(denied, ToolError::Denied { .. }),
        "expected a typed concurrency denial while the worker lives, got {denied:?}"
    );

    // Real termination releases the permit.
    release.store(true, Ordering::SeqCst);
    let _ = wait_for_settlement(&tool).await;
    let second = tool
        .invoke(
            &ctx_with(token, InvocationId::new()),
            json!({ "goal": "after termination" }),
        )
        .await
        .expect("after real termination the permit is released");
    assert_eq!(second.content["outcome"], "completed");
}

#[tokio::test]
async fn reservation_refusal_typed() {
    // A shared session ledger makes reservation refusal REAL: with 100 cents
    // total and the first child holding all of them in flight, a second
    // 100-cent delegation is refused with a typed denial rather than silently
    // double-spending one allowance.
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let provider = gated_provider(&started, &release, 25);
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = Arc::new(
        DelegateTaskTool::with_provider(root, AUDIENCE, provider)
            .with_session_budget(ParentBudget::new(100)),
    );

    let invoke_tool = Arc::clone(&tool);
    let ctx = ctx_with(token.clone(), InvocationId::new());
    let waiter = tokio::spawn(async move {
        invoke_tool
            .invoke(
                &ctx,
                json!({ "goal": "holds the whole ledger", "max_cost_cents": 100 }),
            )
            .await
    });
    wait_for_start(&started).await;

    let refused = tool
        .invoke(
            &ctx_with(token.clone(), InvocationId::new()),
            json!({ "goal": "wants another 100", "max_cost_cents": 100 }),
        )
        .await
        .expect_err("the shared ledger cannot fund two full reservations at once");
    match refused {
        ToolError::Denied { reason } => {
            assert!(
                reason.contains("reservation"),
                "refusal must name the reservation, got: {reason}"
            );
        }
        other => panic!("expected a typed Denied, got {other:?}"),
    }

    // Settlement returns the unspent allowance; a later delegation fits again.
    release.store(true, Ordering::SeqCst);
    let first = waiter
        .await
        .expect("first waiter task panicked")
        .expect("first completes");
    assert_eq!(first.content["outcome"], "completed");
    // After the first child settles its actual spend (25 from the gated mock),
    // 75 remains in the shared 100-cent ledger. Request a budget that fits.
    let second = tool
        .invoke(
            &ctx_with(token, InvocationId::new()),
            json!({ "goal": "fits now", "max_cost_cents": 50 }),
        )
        .await
        .expect("unspent allowance released at settlement funds a later delegation");
    assert_eq!(second.content["outcome"], "completed");
}

#[tokio::test]
async fn zero_budget_is_rejected_before_dispatch() {
    // The schema's `minimum: 1` is not runtime validation; a zero budget would
    // otherwise authorize a real, billable provider round.
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = DelegateTaskTool::with_provider(root, AUDIENCE, echo_provider());
    let err = tool
        .invoke(
            &ctx_with(token, InvocationId::new()),
            json!({ "goal": "zero", "max_cost_cents": 0 }),
        )
        .await
        .expect_err("a zero budget must be refused before any dispatch");
    match err {
        ToolError::InvalidArgs(reason) => {
            assert!(reason.contains(">= 1"), "got: {reason}");
        }
        other => panic!("expected InvalidArgs, got {other:?}"),
    }
}

/// A provider that replies with the request's model id, so tests can prove the
/// configured model is carried into delegated requests.
struct ModelEchoMock;

#[async_trait]
impl Provider for ModelEchoMock {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Ok(CompletionResponse {
            content: req.model.0.clone(),
            finish_reason: FinishReason::Stop,
            usage: Usage {
                tokens_in: 1,
                tokens_out: 1,
                cost_cents: Some(1),
            },
            cost: CostTuple {
                cents: 1,
                ..CostTuple::default()
            },
            raw_provider_response: None,
        })
    }

    fn id(&self) -> ProviderId {
        ProviderId("model-echo".into())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        static CARD: std::sync::OnceLock<RateCard> = std::sync::OnceLock::new();
        CARD.get_or_init(|| RateCard {
            version_id: "model-echo-v1".into(),
            cents_per_1k_input: 0.0,
            cents_per_1k_output: 0.0,
            cents_per_request: 0.0,
        })
    }
}

#[tokio::test]
async fn configured_model_is_carried_into_delegated_requests() {
    let (token, root) = parent_token(&["chat.submit"], 10_000);
    let tool = DelegateTaskTool::with_provider(root, AUDIENCE, Arc::new(ModelEchoMock))
        .with_child_model(ModelId::new("cfg-model-1"));
    let output = tool
        .invoke(
            &ctx_with(token, InvocationId::new()),
            json!({ "goal": "which model" }),
        )
        .await
        .expect("child completes");
    assert_eq!(
        output.content["response"], "cfg-model-1",
        "the configured model must reach the provider request, not a literal 'default'"
    );
}
