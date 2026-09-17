//! ARD-139 — the propose-half of the approval-gate loop:
//! [`FusedRuntime::submit`]'s tool-call stage proposing a pending approval
//! card (and minting a real `approval.propose.created.v1` receipt) for a
//! tool call whose required capability is approval-gated, and honoring an
//! operator's decision on a retried identical call.

mod support;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ardur_approvals::{ApprovalStatus, ApprovalStore, ClaimBinding, ClaimOutcome, Decision};
use ardur_fused_runtime::{load_persisted_chain, verify_persisted_chain};
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, Provider, ProviderError, RateCard, Usage,
};
use ardur_runtime::{ChatRuntime, CostTuple, ProviderId, RuntimeError, SessionId, ToolCall};
use ardur_tool_registry::{
    Capability, Tool, ToolContext, ToolError, ToolId, ToolOutput, ToolRegistry, ToolSchema,
};
use async_trait::async_trait;
use serde_json::json;

use support::{AUDIENCE, HOLDER, TOOL, mint_token_as, request_for, runtime_builder};

/// A tool gated on a single capability, counting invocations so a test can
/// assert a denied/pending call never reached the tool body.
struct GatedTool {
    id: ToolId,
    schema: ToolSchema,
    caps: Vec<Capability>,
    invocations: Arc<AtomicUsize>,
}

impl GatedTool {
    fn new(name: &str, caps: Vec<Capability>, invocations: Arc<AtomicUsize>) -> Self {
        Self {
            id: ToolId::new(name),
            schema: ToolSchema {
                description: "approval-gated tool".to_string(),
                input_schema: json!({ "type": "object" }),
                output_schema: json!({ "type": "object" }),
                examples: vec![],
            },
            caps,
            invocations,
        }
    }
}

#[async_trait]
impl Tool for GatedTool {
    fn id(&self) -> ToolId {
        self.id.clone()
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    async fn invoke(
        &self,
        _ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput {
            content: json!({ "ok": true }),
            cost: CostTuple::default(),
            receipt_data: json!({ "ok": true }),
        })
    }
    fn required_capabilities(&self) -> &[Capability] {
        &self.caps
    }
}

fn gated_registry(
    name: &str,
    caps: Vec<Capability>,
    invocations: Arc<AtomicUsize>,
) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(GatedTool::new(name, caps, invocations)))
        .expect("gated id is unique");
    Arc::new(registry)
}

/// A provider that offers the same gated tool call every round *until* that
/// exact call has actually run (tracked via the tool's own shared
/// `invocations` counter), then settles with a final answer. This models a
/// model that keeps re-requesting a tool it hasn't gotten a result for yet
/// — denied-pending or rejected rounds don't invoke the tool, so it keeps
/// offering the same call across repeated `submit`s over the same session;
/// once the gate lets the call through and the tool actually runs, it stops
/// re-offering it.
struct AlwaysWantsToolProvider {
    tool_name: String,
    args: serde_json::Value,
    invocations: Arc<AtomicUsize>,
    last_seen_invocations: AtomicUsize,
    rate_card: RateCard,
}

impl AlwaysWantsToolProvider {
    fn new(tool_name: &str, args: serde_json::Value, invocations: Arc<AtomicUsize>) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            args,
            invocations,
            last_seen_invocations: AtomicUsize::new(0),
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }
}

#[async_trait]
impl Provider for AlwaysWantsToolProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let current = self.invocations.load(Ordering::SeqCst);
        let last_seen = self.last_seen_invocations.load(Ordering::SeqCst);
        if current > last_seen {
            self.last_seen_invocations.store(current, Ordering::SeqCst);
            return Ok(CompletionResponse {
                content: String::new(),
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
                cost: CostTuple::default(),
                raw_provider_response: None,
            });
        }
        Ok(CompletionResponse {
            content: String::new(),
            finish_reason: FinishReason::ToolUse(vec![ToolCall {
                id: "call_1".to_string(),
                name: self.tool_name.clone(),
                arguments: self.args.clone(),
            }]),
            usage: Usage::default(),
            cost: CostTuple::default(),
            raw_provider_response: None,
        })
    }

    fn id(&self) -> ProviderId {
        ProviderId("always-wants-tool".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

fn gated_caps() -> HashSet<String> {
    HashSet::from([Capability::ShellExec.as_str()])
}

/// A tool call requiring an approval-gated capability is denied with
/// `RuntimeError::ApprovalRequired`, and a `Pending` card is written to the
/// approvals store — never invoking the tool.
#[tokio::test]
async fn gated_call_proposes_a_pending_card_instead_of_invoking_the_tool() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(AlwaysWantsToolProvider::new(
        "gated.shell",
        json!({"cmd": "ls"}),
        invocations.clone(),
    ));
    let approvals_dir = support::tempdir().expect("approvals dir");
    let store = ApprovalStore::new(approvals_dir.path());
    let runtime = runtime_builder(provider)
        .with_tools(gated_registry(
            "gated.shell",
            vec![Capability::ShellExec],
            invocations.clone(),
        ))
        .with_approvals(store.clone())
        .with_approval_gated_capabilities(gated_caps())
        .build()
        .expect("runtime builds");
    let token = mint_token_as(HOLDER, AUDIENCE, &[TOOL, "gated.shell", "cap.shell_exec"]);
    let session_id = SessionId::new();

    let err = runtime
        .submit(request_for("run a command", &token, session_id))
        .await
        .expect_err("an approval-gated call is denied-pending, not allowed");

    let approval_id = match err {
        RuntimeError::ApprovalRequired {
            approval_id, tool, ..
        } => {
            assert_eq!(tool, "gated.shell");
            approval_id
        }
        other => panic!("expected ApprovalRequired, got {other:?}"),
    };
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        0,
        "the gated tool was never invoked"
    );

    let card = store.read(&approval_id).expect("the proposed card exists");
    assert_eq!(card.status, ApprovalStatus::Pending);
    assert_eq!(card.tool, "gated.shell");
}

/// A second identical call while the card is still pending returns the
/// *same* card id rather than proposing a duplicate.
#[tokio::test]
async fn a_retried_identical_call_while_pending_does_not_duplicate_the_card() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(AlwaysWantsToolProvider::new(
        "gated.shell",
        json!({"cmd": "ls"}),
        invocations.clone(),
    ));
    let approvals_dir = support::tempdir().expect("approvals dir");
    let store = ApprovalStore::new(approvals_dir.path());
    let runtime = runtime_builder(provider)
        .with_tools(gated_registry(
            "gated.shell",
            vec![Capability::ShellExec],
            invocations.clone(),
        ))
        .with_approvals(store.clone())
        .with_approval_gated_capabilities(gated_caps())
        .build()
        .expect("runtime builds");
    let token = mint_token_as(HOLDER, AUDIENCE, &[TOOL, "gated.shell", "cap.shell_exec"]);
    let session_id = SessionId::new();

    let first_id = match runtime
        .submit(request_for("run a command", &token, session_id))
        .await
        .expect_err("pending")
    {
        RuntimeError::ApprovalRequired { approval_id, .. } => approval_id,
        other => panic!("expected ApprovalRequired, got {other:?}"),
    };
    let second_id = match runtime
        .submit(request_for("run a command", &token, session_id))
        .await
        .expect_err("still pending")
    {
        RuntimeError::ApprovalRequired { approval_id, .. } => approval_id,
        other => panic!("expected ApprovalRequired, got {other:?}"),
    };

    assert_eq!(
        first_id, second_id,
        "the same pending card is reused, not duplicated"
    );
    assert_eq!(store.list().unwrap().len(), 1, "exactly one card exists");
}

/// The full loop: propose → operator approves (directly against the shared
/// store, standing in for the CLI/HTTP decide-half) → the retried identical
/// call proceeds and the tool actually runs.
#[tokio::test]
async fn approved_card_lets_the_retried_call_proceed() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(AlwaysWantsToolProvider::new(
        "gated.shell",
        json!({"cmd": "ls"}),
        invocations.clone(),
    ));
    let approvals_dir = support::tempdir().expect("approvals dir");
    let store = ApprovalStore::new(approvals_dir.path());
    let runtime = runtime_builder(provider)
        .with_tools(gated_registry(
            "gated.shell",
            vec![Capability::ShellExec],
            invocations.clone(),
        ))
        .with_approvals(store.clone())
        .with_approval_gated_capabilities(gated_caps())
        .build()
        .expect("runtime builds");
    let token = mint_token_as(HOLDER, AUDIENCE, &[TOOL, "gated.shell", "cap.shell_exec"]);
    let session_id = SessionId::new();

    let approval_id = match runtime
        .submit(request_for("run a command", &token, session_id))
        .await
        .expect_err("pending")
    {
        RuntimeError::ApprovalRequired { approval_id, .. } => approval_id,
        other => panic!("expected ApprovalRequired, got {other:?}"),
    };

    // Stand in for an operator approving via `ardur approvals approve` or
    // `POST /approvals/{id}/approve`.
    store
        .decide(&approval_id, Decision::Approve, 1)
        .expect("approve succeeds");

    let outcome = runtime
        .submit(request_for("run a command", &token, session_id))
        .await
        .expect("the approved call now proceeds");

    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "the tool ran exactly once, after approval"
    );
    assert_eq!(outcome.response.content, "");
}

/// A denied card fails the retried call with `ApprovalRejected`, and does
/// not fall back to re-proposing.
#[tokio::test]
async fn denied_card_rejects_the_retried_call() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(AlwaysWantsToolProvider::new(
        "gated.shell",
        json!({"cmd": "ls"}),
        invocations.clone(),
    ));
    let approvals_dir = support::tempdir().expect("approvals dir");
    let store = ApprovalStore::new(approvals_dir.path());
    let runtime = runtime_builder(provider)
        .with_tools(gated_registry(
            "gated.shell",
            vec![Capability::ShellExec],
            invocations.clone(),
        ))
        .with_approvals(store.clone())
        .with_approval_gated_capabilities(gated_caps())
        .build()
        .expect("runtime builds");
    let token = mint_token_as(HOLDER, AUDIENCE, &[TOOL, "gated.shell", "cap.shell_exec"]);
    let session_id = SessionId::new();

    let approval_id = match runtime
        .submit(request_for("run a command", &token, session_id))
        .await
        .expect_err("pending")
    {
        RuntimeError::ApprovalRequired { approval_id, .. } => approval_id,
        other => panic!("expected ApprovalRequired, got {other:?}"),
    };

    store
        .decide(
            &approval_id,
            Decision::Reject {
                reason: "too risky".to_string(),
            },
            1,
        )
        .expect("reject succeeds");

    let err = runtime
        .submit(request_for("run a command", &token, session_id))
        .await
        .expect_err("a rejected call fails, not re-proposes");

    assert!(
        matches!(err, RuntimeError::ApprovalRejected { ref reason, .. } if reason == "too risky"),
        "expected ApprovalRejected with the recorded reason, got {err:?}"
    );
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        0,
        "the rejected tool never ran"
    );
}

/// A runtime with no approvals store configured is unaffected by
/// `approval_gated_capabilities` — the gate is a no-op, matching every other
/// opt-in builder knob.
#[tokio::test]
async fn no_approvals_store_configured_is_a_no_op() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(AlwaysWantsToolProvider::new(
        "gated.shell",
        json!({"cmd": "ls"}),
        invocations.clone(),
    ));
    let runtime = runtime_builder(provider)
        .with_tools(gated_registry(
            "gated.shell",
            vec![Capability::ShellExec],
            invocations.clone(),
        ))
        .with_approval_gated_capabilities(gated_caps())
        .build()
        .expect("runtime builds");
    let token = mint_token_as(HOLDER, AUDIENCE, &[TOOL, "gated.shell", "cap.shell_exec"]);

    let outcome = runtime
        .submit(request_for("run a command", &token, SessionId::new()))
        .await
        .expect("no approvals store means the gate never fires");

    assert_eq!(invocations.load(Ordering::SeqCst), 1);
    assert_eq!(outcome.response.content, "");
}

/// A propose receipt is a real, signed receipt chained onto the same log a
/// turn receipt uses.
#[tokio::test]
async fn propose_receipt_chains_with_turn_receipts() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(AlwaysWantsToolProvider::new(
        "gated.shell",
        json!({"cmd": "ls"}),
        invocations.clone(),
    ));
    let approvals_dir = support::tempdir().expect("approvals dir");
    let store = ApprovalStore::new(approvals_dir.path());
    let receipt_log = tempfile::NamedTempFile::new_in(approvals_dir.path()).expect("receipt log");
    let runtime = runtime_builder(provider)
        .with_tools(gated_registry(
            "gated.shell",
            vec![Capability::ShellExec],
            invocations.clone(),
        ))
        .with_approvals(store.clone())
        .with_approval_gated_capabilities(gated_caps())
        .receipt_log(receipt_log.path())
        .build()
        .expect("runtime builds");
    let token = mint_token_as(HOLDER, AUDIENCE, &[TOOL, "gated.shell", "cap.shell_exec"]);
    let session_id = SessionId::new();

    let approval_id = match runtime
        .submit(request_for("run a command", &token, session_id))
        .await
        .expect_err("pending")
    {
        RuntimeError::ApprovalRequired { approval_id, .. } => approval_id,
        other => panic!("expected ApprovalRequired, got {other:?}"),
    };
    store
        .decide(&approval_id, Decision::Approve, 1)
        .expect("approve succeeds");
    let turn = runtime
        .submit(request_for("run a command", &token, session_id))
        .await
        .expect("approved call proceeds");

    // One propose receipt from the first (denied-pending) submit, then two
    // turn receipts from the second (approved) submit — a tool-call round
    // and the settling final-answer round each mint their own receipt, per
    // the pipeline's per-provider-round commit (see
    // `crates/fused-runtime/tests/tool_execution.rs`'s `single_round_trip`).
    let chain = load_persisted_chain(receipt_log.path()).expect("chain loads");
    assert_eq!(chain.len(), 3);
    assert_eq!(chain[0].body.verb.as_str(), "approval.propose.created.v1");
    assert_eq!(chain.last().unwrap().body.receipt_id, turn.receipt_id.0);
    verify_persisted_chain(&chain).expect("the chain verifies");
}

/// An approval authorises **one** invocation, not a standing permission.
///
/// Without consumption, `find_matching` keeps returning the same `Approved`
/// card and every later identical call is waved through on a grant the operator
/// already spent — for `shell.run` or `file.write` that is a materially larger
/// authorisation than the one given.
#[tokio::test]
async fn an_approved_card_authorises_one_call_and_is_then_spent() {
    let root = support::tempdir().expect("tempdir");
    let store = ApprovalStore::new(root.path().join("approvals"));

    let card = store
        .propose(
            "echo",
            Capability::ShellExec.as_str(),
            "digest-abc",
            Some("session-1".to_string()),
            "gated for test",
            1_700_000_000,
        )
        .expect("propose");
    let id = card.id.clone().expect("card id");
    store
        .decide(&id, Decision::Approve, 1_700_000_001)
        .expect("approve");

    // The authorised call finds it.
    let found = store
        .find_matching("echo", "digest-abc", Some("session-1"))
        .expect("lookup")
        .expect("an approved card matches before it is spent");
    assert_eq!(found.status, ApprovalStatus::Approved);

    // Spending it.
    let binding = ClaimBinding {
        tool: "echo",
        arguments_digest: "digest-abc",
        session_id: Some("session-1"),
        claimed_by: Some("ardur:test"),
    };
    let claimed = store
        .claim_execution(&id, &binding, 1_700_000_002)
        .expect("claim an approved card");
    assert!(claimed.is_won(), "the first claim grants the execution");
    assert_eq!(claimed.into_card().status, ApprovalStatus::Consumed);

    // The next identical call must NOT find it — it has to ask again.
    assert!(
        store
            .find_matching("echo", "digest-abc", Some("session-1"))
            .expect("lookup")
            .is_none(),
        "a spent approval must stop matching, or one approval becomes an \
         unlimited standing permission to repeat the call"
    );

    // A repeated claim is an idempotent OBSERVATION, not a fresh grant.
    let second = store
        .claim_execution(&id, &binding, 1_700_000_003)
        .expect("a repeated claim is observable");
    assert!(
        matches!(second, ClaimOutcome::AlreadySpent(_)),
        "a retry observes the spent card without regaining authority"
    );

    // The record survives for audit: the receipt chain references this id.
    assert_eq!(
        store.read(&id).expect("still readable").status,
        ApprovalStatus::Consumed
    );
}

/// Claiming a card that was never approved would invent an authorisation.
#[tokio::test]
async fn a_pending_or_denied_card_cannot_be_claimed() {
    let root = support::tempdir().expect("tempdir");
    let store = ApprovalStore::new(root.path().join("approvals"));

    let pending = store
        .propose(
            "echo",
            Capability::ShellExec.as_str(),
            "digest-pending",
            None,
            "gated",
            1_700_000_000,
        )
        .expect("propose");
    let pending_id = pending.id.clone().expect("id");
    let pending_binding = ClaimBinding {
        tool: "echo",
        arguments_digest: "digest-pending",
        session_id: None,
        claimed_by: Some("ardur:test"),
    };
    assert!(
        store
            .claim_execution(&pending_id, &pending_binding, 1_700_000_001)
            .is_err(),
        "claiming a pending card would authorise a call no operator approved"
    );

    let denied = store
        .propose(
            "echo",
            Capability::ShellExec.as_str(),
            "digest-denied",
            None,
            "gated",
            1_700_000_000,
        )
        .expect("propose");
    let denied_id = denied.id.clone().expect("id");
    store
        .decide(
            &denied_id,
            Decision::Reject {
                reason: "no".to_string(),
            },
            1_700_000_001,
        )
        .expect("reject");
    let denied_binding = ClaimBinding {
        tool: "echo",
        arguments_digest: "digest-denied",
        session_id: None,
        claimed_by: Some("ardur:test"),
    };
    assert!(
        store
            .claim_execution(&denied_id, &denied_binding, 1_700_000_002)
            .is_err(),
        "claiming a denied card would reverse the operator's rejection"
    );
}

// ---------------------------------------------------------------------------
// gh#497 — claim-once at the authorization-to-effect boundary
// ---------------------------------------------------------------------------

/// A gated tool that blocks inside `invoke` until released, so a test can
/// hold one admitted call open while a second identical call arrives. The
/// invocation counter increments only AFTER release, so the provider below
/// keeps offering the tool while the first call is still in flight.
struct BlockingTool {
    id: ToolId,
    schema: ToolSchema,
    invocations: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl BlockingTool {
    fn new(
        name: &str,
        invocations: Arc<AtomicUsize>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    ) -> Self {
        Self {
            id: ToolId::new(name),
            schema: ToolSchema {
                description: "approval-gated blocking tool".to_string(),
                input_schema: json!({ "type": "object" }),
                output_schema: json!({ "type": "object" }),
                examples: vec![],
            },
            invocations,
            entered,
            release,
        }
    }
}

#[async_trait]
impl Tool for BlockingTool {
    fn id(&self) -> ToolId {
        self.id.clone()
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    async fn invoke(
        &self,
        _ctx: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        self.entered.notify_one();
        self.release.notified().await;
        self.invocations.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput {
            content: json!({ "ok": true }),
            cost: CostTuple::default(),
            receipt_data: json!({ "ok": true }),
        })
    }
    fn required_capabilities(&self) -> &[Capability] {
        const CAPS: &[Capability] = &[Capability::ShellExec];
        CAPS
    }
}

/// The exactly-one-effect proof: two overlapping admitted calls over the
/// SAME approved card produce exactly one tool invocation. The winner runs;
/// the loser is denied with a FRESH pending card (its own approval to wait
/// for), never a ride on the spent grant.
#[tokio::test]
async fn overlapping_admitted_calls_have_exactly_one_effect() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = Arc::new(AlwaysWantsToolProvider::new(
        "gated.shell",
        json!({"cmd": "ls"}),
        invocations.clone(),
    ));
    let approvals_dir = support::tempdir().expect("approvals dir");
    let store = ApprovalStore::new(approvals_dir.path());
    let receipt_log = tempfile::NamedTempFile::new_in(approvals_dir.path()).expect("receipt log");

    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(BlockingTool::new(
            "gated.shell",
            invocations.clone(),
            entered.clone(),
            release.clone(),
        )))
        .expect("gated id is unique");
    let registry = Arc::new(registry);
    // Each runtime permits one economic execution. Race the shared real
    // approval store across two independent owners, not a capacity rejection.
    let runtime_b = runtime_builder(provider.clone())
        .with_tools(registry.clone())
        .with_approvals(store.clone())
        .with_approval_gated_capabilities(gated_caps())
        .build()
        .expect("second runtime builds");
    let runtime = runtime_builder(provider)
        .with_tools(registry)
        .with_approvals(store.clone())
        .with_approval_gated_capabilities(gated_caps())
        .receipt_log(receipt_log.path())
        // Enough budget for TWO concurrently open per-turn envelopes: call A
        // holds its reservation while blocked in the tool, and call B's
        // admission must not fail on budget before it can reach the gate.
        .provision_budget(
            support::gate_holder(),
            ardur_cost_gate::CostTuple {
                tokens_in: 1_000_000_000,
                tokens_out: 1_000_000_000,
                cents: 4_000_000,
                wall_ms: 1_000_000_000,
                attention_score: 1_000_000_000,
            },
        )
        .build()
        .expect("runtime builds");
    let token = mint_token_as(HOLDER, AUDIENCE, &[TOOL, "gated.shell", "cap.shell_exec"]);
    let session_id = SessionId::new();

    // Propose + approve the card both calls will race over.
    let approval_id = match runtime
        .submit(request_for("run a command", &token, session_id))
        .await
        .expect_err("pending")
    {
        RuntimeError::ApprovalRequired { approval_id, .. } => approval_id,
        other => panic!("expected ApprovalRequired, got {other:?}"),
    };
    store
        .decide(&approval_id, Decision::Approve, 1)
        .expect("approve succeeds");

    // Call A enters the tool and blocks; call B then arrives over the same
    // card. join! drives both cooperatively on this one runtime.
    let call_a = runtime.submit(request_for("run a command", &token, session_id));
    let controller = async {
        entered.notified().await;
        // A is now inside the tool, holding the spent card. B must NOT also
        // invoke: it gets its own pending card instead.
        let b_err = runtime_b
            .submit(request_for("run a command", &token, session_id))
            .await
            .expect_err("the overlapping call must not proceed on a spent grant");
        let b_approval_id = match b_err {
            RuntimeError::ApprovalRequired { approval_id, .. } => approval_id,
            other => panic!("expected ApprovalRequired for the loser, got {other:?}"),
        };
        assert_ne!(
            b_approval_id, approval_id,
            "the loser waits on its OWN card, not the spent one"
        );
        let b_card = store.read(&b_approval_id).expect("the loser's card exists");
        assert_eq!(b_card.status, ApprovalStatus::Pending);
        release.notify_one();
    };
    let (a_result, ()) = tokio::join!(call_a, controller);
    a_result.expect("the winner's turn completes");

    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "exactly one effect under overlapping admitted calls"
    );

    // The winner's claim is bound and recorded: caller identity + completed.
    let won = store
        .read(&approval_id)
        .expect("the spent card is readable");
    assert_eq!(won.status, ApprovalStatus::Consumed);
    assert_eq!(
        won.consumed_by.as_deref(),
        Some(HOLDER),
        "the claim records the verified caller, not an anonymous spender"
    );
    assert_eq!(
        won.invocation_outcome.as_ref().map(|o| o.result),
        Some(ardur_approvals::InvocationResult::Completed),
        "the resolved invocation records its outcome"
    );

    // Every minted receipt — the first propose, the winner's turn, and the
    // loser's fresh propose — chains and verifies.
    let chain = load_persisted_chain(receipt_log.path()).expect("chain loads");
    assert!(
        chain.len() >= 2,
        "propose + turn receipts are present, got {}",
        chain.len()
    );
    verify_persisted_chain(&chain).expect("the chain verifies");
}

/// A failed gated invocation stays spent (consume-before-invoke) and records
/// the failure — a failed tool is never recorded as a success, and the spent
/// grant cannot be retried.
#[tokio::test]
async fn a_failed_gated_invocation_is_recorded_failed_and_stays_spent() {
    struct FailingTool;
    #[async_trait]
    impl Tool for FailingTool {
        fn id(&self) -> ToolId {
            ToolId::new("gated.failing")
        }
        fn schema(&self) -> &ToolSchema {
            static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
            SCHEMA.get_or_init(|| ToolSchema {
                description: "approval-gated failing tool".to_string(),
                input_schema: json!({ "type": "object" }),
                output_schema: json!({ "type": "object" }),
                examples: vec![],
            })
        }
        async fn invoke(
            &self,
            _ctx: &ToolContext,
            _args: serde_json::Value,
        ) -> Result<ToolOutput, ToolError> {
            Err(ToolError::ExecutionFailed("boom".to_string()))
        }
        fn required_capabilities(&self) -> &[Capability] {
            const CAPS: &[Capability] = &[Capability::ShellExec];
            CAPS
        }
    }

    /// Offers `gated.failing` once per submit so each attempt reaches the gate.
    struct FailingProvider;
    #[async_trait]
    impl Provider for FailingProvider {
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            Ok(CompletionResponse {
                content: String::new(),
                finish_reason: FinishReason::ToolUse(vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "gated.failing".to_string(),
                    arguments: json!({"cmd": "ls"}),
                }]),
                usage: Usage::default(),
                cost: CostTuple::default(),
                raw_provider_response: None,
            })
        }
        fn id(&self) -> ProviderId {
            ProviderId("failing-provider".to_string())
        }
        fn supports_streaming(&self) -> bool {
            false
        }
        fn rate_card(&self) -> &RateCard {
            static RATE: std::sync::OnceLock<RateCard> = std::sync::OnceLock::new();
            RATE.get_or_init(RateCard::anthropic_2026_q2_v1)
        }
    }

    let approvals_dir = support::tempdir().expect("approvals dir");
    let store = ApprovalStore::new(approvals_dir.path());
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(FailingTool)).expect("id unique");
    let runtime = runtime_builder(Arc::new(FailingProvider))
        .with_tools(Arc::new(registry))
        .with_approvals(store.clone())
        .with_approval_gated_capabilities(gated_caps())
        .build()
        .expect("runtime builds");
    let token = mint_token_as(HOLDER, AUDIENCE, &[TOOL, "gated.failing", "cap.shell_exec"]);
    let session_id = SessionId::new();

    let approval_id = match runtime
        .submit(request_for("run it", &token, session_id))
        .await
        .expect_err("pending")
    {
        RuntimeError::ApprovalRequired { approval_id, .. } => approval_id,
        other => panic!("expected ApprovalRequired, got {other:?}"),
    };
    store
        .decide(&approval_id, Decision::Approve, 1)
        .expect("approve succeeds");

    // The approved call runs the tool, which fails: the submit fails, the
    // card stays spent, and the outcome records `failed`.
    runtime
        .submit(request_for("run it", &token, session_id))
        .await
        .expect_err("the failing tool fails the turn");
    let card = store.read(&approval_id).expect("card readable");
    assert_eq!(card.status, ApprovalStatus::Consumed);
    assert_eq!(
        card.invocation_outcome.as_ref().map(|o| o.result),
        Some(ardur_approvals::InvocationResult::Failed),
        "a failed tool is recorded as failed"
    );

    // The spent grant cannot be retried: the next identical call needs a
    // fresh approval.
    let next = runtime
        .submit(request_for("run it", &token, session_id))
        .await
        .expect_err("a spent grant does not re-authorize");
    match next {
        RuntimeError::ApprovalRequired {
            approval_id: fresh, ..
        } => assert_ne!(fresh, approval_id, "a fresh pending card is proposed"),
        other => panic!("expected ApprovalRequired, got {other:?}"),
    }
}

/// A timed-out gated invocation leaves the spent card WITHOUT an outcome —
/// the explicit ambiguous-effect state, never a fabricated resolution.
#[tokio::test]
async fn a_timed_out_gated_invocation_leaves_the_effect_ambiguous() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = Arc::new(AlwaysWantsToolProvider::new(
        "gated.shell",
        json!({"cmd": "ls"}),
        invocations.clone(),
    ));
    let approvals_dir = support::tempdir().expect("approvals dir");
    let store = ApprovalStore::new(approvals_dir.path());

    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(BlockingTool::new(
            "gated.shell",
            invocations.clone(),
            entered.clone(),
            release.clone(),
        )))
        .expect("gated id is unique");
    let runtime = runtime_builder(provider)
        .with_tools(Arc::new(registry))
        .with_approvals(store.clone())
        .with_approval_gated_capabilities(gated_caps())
        .tool_timeout(std::time::Duration::from_millis(50))
        .build()
        .expect("runtime builds");
    let token = mint_token_as(HOLDER, AUDIENCE, &[TOOL, "gated.shell", "cap.shell_exec"]);
    let session_id = SessionId::new();

    let approval_id = match runtime
        .submit(request_for("run a command", &token, session_id))
        .await
        .expect_err("pending")
    {
        RuntimeError::ApprovalRequired { approval_id, .. } => approval_id,
        other => panic!("expected ApprovalRequired, got {other:?}"),
    };
    store
        .decide(&approval_id, Decision::Approve, 1)
        .expect("approve succeeds");

    // The tool blocks forever (never released); the 50ms timeout fires.
    let err = runtime
        .submit(request_for("run a command", &token, session_id))
        .await
        .expect_err("the hung tool times the call out");
    assert!(
        matches!(err, RuntimeError::ToolTimeout { .. }),
        "expected ToolTimeout, got {err:?}"
    );

    let card = store.read(&approval_id).expect("card readable");
    assert_eq!(
        card.status,
        ApprovalStatus::Consumed,
        "the grant stays spent — consume-before-invoke"
    );
    assert!(
        card.invocation_outcome.is_none(),
        "no resolution is fabricated for an unresolved call: {:?}",
        card.invocation_outcome
    );
    assert_eq!(
        ApprovalStore::effect_state_of(&card),
        ardur_approvals::EffectState::Ambiguous,
        "the explicit ambiguous-effect state"
    );
    // Unblock the tool so the test runtime can drop cleanly.
    release.notify_one();
}
