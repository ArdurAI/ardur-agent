//! `delegate_task` — the parent-facing spawn primitive over the §5.0/§5.1
//! sub-agent substrate (`ardur-multi-agent`).
//!
//! # Why this crate exists
//!
//! `ardur-multi-agent` ships `MultiAgentRuntime` (spawn/ask/terminate over an
//! attenuated cap-token, an isolated budget, and a chained termination
//! receipt) but is not itself a [`Tool`] — nothing in the fused runtime's tool
//! loop could reach it. This crate is the thin [`Tool`] adapter: it decodes a
//! model tool-call into a sub-agent spawn, drives it to completion, and folds
//! the [`TerminationReceipt`] into the [`ToolOutput`] the fused runtime feeds
//! back to the model.
//!
//! # Adaptation, not the full §5.1 blueprint
//!
//! `plans/5.1-delegate-task-tool-child-mission-derivation-blueprint.md`
//! specifies a considerably larger surface (`ChildMissionPassport`,
//! `MultiAgentCoordinator` facade, batch mode, idempotency keys, eleven
//! derivation invariants) built against a `crates/subagent/` substrate that
//! does not exist in this tree. This crate instead adapts the existing,
//! already-tested `ardur-multi-agent` vocabulary (`SubAgentSpec`,
//! `MultiAgentRuntime`, `TerminationReceipt`) to the existing `Tool` trait, so
//! the crate that already delivers attenuation-with-teeth and receipt
//! chaining is reachable from a real tool call instead of only its own tests.
//!
//! # Recursion is denied structurally, not by a denylist
//!
//! Every spawned child's cap-token is attenuated with
//! [`AttenuationRule::RestrictTools`] down to exactly
//! [`CHAT_SUBMIT_TOOL`] (`"chat.submit"`). Because Biscuit attenuation only
//! ever narrows, the child's token carries neither `delegate_task`'s tool id
//! nor its `cap.*` capability label — so a child cannot itself call
//! `delegate_task` no matter what the parent's system prompt says. This
//! mirrors Hermes's `DELEGATE_BLOCKED_TOOLS` precedent
//! (`delegate_task` is always stripped) using the token's own attenuation
//! algebra instead of a Python frozenset.
//!
//! # Receipt chaining
//!
//! The [`TerminationReceipt::parent_receipt_id`] this tool emits is the
//! calling [`ToolContext::invocation_id`] reinterpreted as a
//! [`ReceiptId`](ardur_runtime::ReceiptId). The fused runtime folds this same
//! `invocation_id` into the `ToolCallReceipt` it emits for the `delegate_task`
//! call itself, so an auditor can walk from the parent's tool-call receipt to
//! the child's termination receipt by matching that one id — the delegation
//! chain is reconstructible without a separate registry.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use ardur_cap_token::{AttenuationRule, CapToken, CapTokenError, PublicKey};
use ardur_cost_gate::CostEnvelope;
use ardur_multi_agent::{
    AgentId, CHAT_SUBMIT_TOOL, InMemoryMultiAgentRuntime, MultiAgentError, MultiAgentRuntime,
    SubAgentRequest, SubAgentSpec, TerminationReason, TerminationReceipt,
};
use ardur_receipt::CostTuple as ReceiptCostTuple;
use ardur_runtime::{ChatMessage, CostTuple, ReceiptId, SessionId};
use ardur_tool_registry::{
    Capability, Tool, ToolContext, ToolError, ToolId, ToolOutput, ToolSchema,
};

/// Conservative default lifetime spend ceiling for a delegated sub-agent, in US
/// cents, applied when a `delegate_task` call omits `max_cost_cents`.
pub const DEFAULT_MAX_COST_CENTS: u32 = 100;

/// The capability label gating `delegate_task`. Included automatically in a
/// session's cap-token allowlist once this tool is registered (every
/// registered tool's `required_capabilities` labels are folded into the
/// session token — see `ardur-server`'s `tool_allowlist_for_runtime`), so no
/// separate grant step is needed to make delegation reachable.
const DELEGATE_CAPABILITY: &str = "multi_agent_delegate";

/// The model-facing arguments a `delegate_task` call decodes into.
///
/// A deliberately small subset of the full §5.1 blueprint's
/// `DelegateTaskArgs` (no batch mode, no toolset hints, no idempotency key —
/// see the crate-level docs for why): just enough to spawn one child with a
/// bounded budget.
#[derive(Debug, Deserialize)]
struct DelegateTaskArgs {
    /// The task the sub-agent should complete.
    goal: String,
    /// Short operator-facing label folded into the sub-agent's id.
    #[serde(default)]
    task_name: Option<String>,
    /// Override of the sub-agent's lifetime spend ceiling, in US cents.
    #[serde(default)]
    max_cost_cents: Option<u32>,
}

/// Spawns a bounded child agent under a cap-token attenuated from the calling
/// session's own authority, and returns its answer once it completes.
///
/// See the crate-level docs for the attenuation and receipt-chaining
/// contract.
pub struct DelegateTaskTool {
    schema: ToolSchema,
    capabilities: Vec<Capability>,
    /// The issuer root every session cap-token (and so every `delegate_task`
    /// caller's token) verifies against. Must be the same root the caller's
    /// `ctx.cap_token` was issued under, or every spawn fails cap-token
    /// verification.
    root: PublicKey,
    /// The audience session cap-tokens are scoped to (e.g. `"ardur"`). Must
    /// match the audience the caller's token was issued for.
    audience: String,
}

impl DelegateTaskTool {
    /// The id this tool registers under.
    pub const ID: &'static str = "delegate_task";

    /// Build a `delegate_task` tool that attenuates and verifies against
    /// `root`, checking spawned children's turns against `audience`.
    #[must_use]
    pub fn new(root: PublicKey, audience: impl Into<String>) -> Self {
        let schema = ToolSchema {
            description: "Spawn a bounded child agent under a cap-token attenuated from this \
                agent's own authority, and return its answer once it completes. The child cannot \
                itself delegate (recursion is denied by attenuation, not policy) and cannot exceed \
                the given cost ceiling."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "The task the child agent should complete."
                    },
                    "task_name": {
                        "type": "string",
                        "description": "Optional short operator-facing label for this delegation."
                    },
                    "max_cost_cents": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional override of the child's lifetime spend ceiling, \
                            in US cents. Defaults to 100."
                    }
                },
                "required": ["goal"]
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "child_agent_id": { "type": "string" },
                    "outcome": { "type": "string", "enum": ["completed", "failed"] },
                    "response": { "type": "string" },
                    "cents_used": { "type": "integer" },
                    "parent_receipt_id": { "type": "string" },
                    "termination_receipt_id": { "type": "string" }
                },
                "required": ["child_agent_id", "outcome", "termination_receipt_id"]
            }),
            examples: vec![],
        };
        Self {
            schema,
            capabilities: vec![Capability::Custom(DELEGATE_CAPABILITY.to_string())],
            root,
            audience: audience.into(),
        }
    }
}

#[async_trait]
impl Tool for DelegateTaskTool {
    fn id(&self) -> ToolId {
        ToolId::new(Self::ID)
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let args: DelegateTaskArgs =
            serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
        if args.goal.trim().is_empty() {
            return Err(ToolError::InvalidArgs(
                "`goal` must not be empty".to_string(),
            ));
        }

        let request = DelegationWorkerRequest {
            root: self.root,
            audience: self.audience.clone(),
            parent_cap_token: ctx.cap_token.0.clone(),
            parent_receipt_id: ctx.invocation_id.0,
            parent_session_id: ctx.session_id,
            goal: args.goal,
            task_name: args.task_name,
            max_cost_cents: args.max_cost_cents.unwrap_or(DEFAULT_MAX_COST_CENTS),
        };

        // `MultiAgentRuntime` is `#[async_trait(?Send)]` (its child `ChatRuntime`
        // future carries no Send bound), but `Tool::invoke`'s future must be
        // Send. Run the whole spawn/ask/terminate sequence on a dedicated
        // blocking-pool thread with its own single-threaded runtime + LocalSet,
        // so the non-Send future never needs to cross a thread boundary — only
        // the plain-data request in and the plain-data outcome out do.
        let outcome = tokio::task::spawn_blocking(move || run_delegation(request))
            .await
            .map_err(|e| {
                ToolError::Internal(anyhow::anyhow!("delegate_task worker panicked: {e}"))
            })??;

        Ok(outcome.into_tool_output())
    }

    fn required_capabilities(&self) -> &[Capability] {
        &self.capabilities
    }
}

/// The plain-data request handed to the blocking worker — every field is
/// `Send + 'static` so it can cross into `spawn_blocking`.
struct DelegationWorkerRequest {
    root: PublicKey,
    audience: String,
    parent_cap_token: String,
    parent_receipt_id: Uuid,
    parent_session_id: SessionId,
    goal: String,
    task_name: Option<String>,
    max_cost_cents: u32,
}

/// The plain-data result of a completed delegation, folded into a
/// [`ToolOutput`] back on the async side.
struct DelegationOutcome {
    child_agent_id: AgentId,
    completed: bool,
    response_text: String,
    cost_used: ReceiptCostTuple,
    receipt: TerminationReceipt,
}

impl DelegationOutcome {
    fn into_tool_output(self) -> ToolOutput {
        let content = json!({
            "child_agent_id": self.child_agent_id.0,
            "outcome": if self.completed { "completed" } else { "failed" },
            "response": self.response_text,
            "cents_used": self.receipt.total_cost.cents,
            "parent_receipt_id": self.receipt.parent_receipt_id.0.to_string(),
            "termination_receipt_id": self.receipt.receipt_id.0.to_string(),
        });
        let receipt_data = json!({
            "child_agent_id": self.child_agent_id.0,
            "termination_receipt_id": self.receipt.receipt_id.0.to_string(),
            "parent_receipt_id": self.receipt.parent_receipt_id.0.to_string(),
            "reason": termination_reason_label(&self.receipt.reason),
            "total_cost_cents": self.receipt.total_cost.cents,
        });
        ToolOutput {
            content,
            cost: CostTuple {
                tokens_in: self.cost_used.tokens_in,
                tokens_out: self.cost_used.tokens_out,
                cents: self.cost_used.cents,
                wall_ms: self.cost_used.wall_ms,
                attention_score: self.cost_used.attention_score,
            },
            receipt_data,
        }
    }
}

/// A zero-cost [`ReceiptCostTuple`] — `ardur_receipt::CostTuple` derives
/// neither `Default` nor `Copy`, so the failed-turn path builds one by hand.
fn zero_cost() -> ReceiptCostTuple {
    ReceiptCostTuple {
        tokens_in: 0,
        tokens_out: 0,
        cents: 0,
        wall_ms: 0,
        attention_score: 0.0,
    }
}

fn termination_reason_label(reason: &TerminationReason) -> &'static str {
    match reason {
        TerminationReason::Completed => "completed",
        TerminationReason::BudgetExhausted => "budget_exhausted",
        TerminationReason::TimedOut { .. } => "timed_out",
        TerminationReason::Cancelled { .. } => "cancelled",
        TerminationReason::ErrorOccurred(_) => "error",
    }
}

/// Run one full spawn -> ask -> terminate sequence to completion on the
/// current (blocking-pool) thread, inside a dedicated single-threaded runtime.
fn run_delegation(req: DelegationWorkerRequest) -> Result<DelegationOutcome, ToolError> {
    let parent_token =
        CapToken::from_base64(&req.parent_cap_token, &req.root).map_err(cap_token_denied)?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            ToolError::Internal(anyhow::anyhow!(
                "building delegate_task worker runtime: {e}"
            ))
        })?;
    let local = tokio::task::LocalSet::new();

    local.block_on(&rt, drive_delegation(req, parent_token))
}

async fn drive_delegation(
    req: DelegationWorkerRequest,
    parent_token: CapToken,
) -> Result<DelegationOutcome, ToolError> {
    let runtime = InMemoryMultiAgentRuntime::verifying(
        req.audience,
        parent_token,
        req.root,
        ReceiptId(req.parent_receipt_id),
    );

    let label = req.task_name.unwrap_or_else(|| "delegate".to_string());
    let agent_id = AgentId::new(format!("{label}-{}", Uuid::new_v4()));

    let spec = SubAgentSpec {
        agent_id: agent_id.clone(),
        goal: req.goal.clone(),
        // The non-weakenable recursive-deny floor: the child's only tool
        // capability is `chat.submit`, so it structurally cannot reach
        // `delegate_task` (or any other tool) itself.
        cap_token_attenuation: vec![AttenuationRule::RestrictTools(vec![
            CHAT_SUBMIT_TOOL.to_string(),
        ])],
        cost_envelope: CostEnvelope {
            cents_max: req.max_cost_cents,
            ..CostEnvelope::default()
        },
        parent_session_id: req.parent_session_id,
    };

    let handle = runtime.spawn(spec).await.map_err(multi_agent_denied)?;

    let ask_result = runtime
        .ask(
            &handle,
            SubAgentRequest {
                message: ChatMessage::user(req.goal),
                max_cost_cents: req.max_cost_cents,
            },
        )
        .await;

    let (reason, completed, response_text, cost_used) = match &ask_result {
        Ok(resp) => (
            TerminationReason::Completed,
            true,
            resp.message.content.clone(),
            resp.cost_used.clone(),
        ),
        Err(e) => (
            TerminationReason::ErrorOccurred(e.to_string()),
            false,
            String::new(),
            zero_cost(),
        ),
    };

    let receipt = runtime
        .terminate(handle, reason)
        .await
        .map_err(multi_agent_denied)?;

    if let Err(e) = ask_result {
        return Err(ToolError::ExecutionFailed(format!(
            "sub-agent {agent_id} turn failed: {e} (termination receipt {} recorded)",
            receipt.receipt_id.0
        )));
    }

    Ok(DelegationOutcome {
        child_agent_id: agent_id,
        completed,
        response_text,
        cost_used,
        receipt,
    })
}

fn cap_token_denied(err: CapTokenError) -> ToolError {
    ToolError::Denied {
        reason: format!("parent cap-token invalid or expired: {err}"),
    }
}

fn multi_agent_denied(err: MultiAgentError) -> ToolError {
    match err {
        MultiAgentError::BudgetExhausted {
            agent,
            used,
            envelope,
        } => ToolError::Denied {
            reason: format!(
                "sub-agent {agent} budget exhausted: used {used}c of {envelope}c envelope"
            ),
        },
        MultiAgentError::CapTokenError(e) => ToolError::Denied {
            reason: format!("sub-agent cap-token attenuation/authorization failed: {e}"),
        },
        MultiAgentError::AgentNotFound(id) => {
            ToolError::Internal(anyhow::anyhow!("sub-agent {id} not found"))
        }
        MultiAgentError::AlreadyTerminated(id) => {
            ToolError::Internal(anyhow::anyhow!("sub-agent {id} already terminated"))
        }
        MultiAgentError::Runtime(e) => {
            ToolError::ExecutionFailed(format!("child runtime rejected the turn: {e}"))
        }
        MultiAgentError::Internal(e) => ToolError::Internal(e),
    }
}
