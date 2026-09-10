//! ARD-139 — the propose-half of the approval-gate loop:
//! [`FusedRuntime::submit`]'s tool-call stage proposing a pending approval
//! card (and minting a real `approval.propose.created.v1` receipt) for a
//! tool call whose required capability is approval-gated, and honoring an
//! operator's decision on a retried identical call.

mod support;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ardur_approvals::{ApprovalStatus, ApprovalStore, Decision};
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
    let approvals_dir = tempfile::tempdir().expect("approvals dir");
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
    let approvals_dir = tempfile::tempdir().expect("approvals dir");
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
    let approvals_dir = tempfile::tempdir().expect("approvals dir");
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
    let approvals_dir = tempfile::tempdir().expect("approvals dir");
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
    let approvals_dir = tempfile::tempdir().expect("approvals dir");
    let store = ApprovalStore::new(approvals_dir.path());
    let receipt_log = tempfile::NamedTempFile::new().expect("receipt log");
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
