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
//!
//! # Concurrency ceiling
//!
//! At most [`DEFAULT_MAX_CONCURRENCY`] `delegate_task` calls may be in flight
//! at once per tool instance — the blueprint's own default budget envelope
//! ("Hermes-defaults applied: `max_concurrency = 3`, `max_depth = 1`",
//! `plans/5.0-multi-agent-foundation-blueprint.md:1714`). Depth is already
//! structurally 1 via the recursion floor above; this is the concurrency
//! half. A call beyond the ceiling is denied immediately (fail-fast) rather
//! than queued, matching the blueprint's default `ConcurrencyOverflowPosture`.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;
use uuid::Uuid;

use ardur_cap_token::{
    AttenuationRule, BiscuitCapTokenAttenuator, CapToken, CapTokenAttenuator, CapTokenError,
    DenyList, HashSetDenyList, PublicKey,
};
use ardur_cost_gate::CostEnvelope;
use ardur_delegate_child::{
    ChildError, ChildOutcome, ChildSpec, ChildSupervisor, ParentBudget, TokenAuthority,
};
use ardur_multi_agent::{AgentId, CHAT_SUBMIT_TOOL, TerminationReason, TerminationReceipt};
use ardur_provider_runtime::{ModelId, Provider};
use ardur_receipt::CostTuple as ReceiptCostTuple;
use ardur_runtime::{CostTuple, ReceiptId};
use ardur_tool_registry::{
    Capability, Tool, ToolContext, ToolError, ToolId, ToolOutput, ToolSchema,
};

/// Conservative default lifetime spend ceiling for a delegated sub-agent, in US
/// cents, applied when a `delegate_task` call omits `max_cost_cents`.
pub const DEFAULT_MAX_COST_CENTS: u32 = 100;

/// Default cap on `delegate_task` calls in flight at once for a single tool
/// instance (one per process). §5.0/§5.1's own "Included" scope names this
/// exact default: Hermes's `_DEFAULT_MAX_CONCURRENT_CHILDREN = 3`
/// (`delegate_tool.py:127`), reaffirmed as the blueprint's default budget
/// envelope ("Hermes-defaults applied: `max_concurrency = 3`,
/// `max_depth = 1`" — `plans/5.0-multi-agent-foundation-blueprint.md:1714`).
/// Depth is already structurally 1 in this crate (a child's token carries
/// only `chat.submit`, so it cannot itself delegate); this is the
/// concurrency half of that same default.
pub const DEFAULT_MAX_CONCURRENCY: usize = 3;

/// The capability label gating `delegate_task`. Included automatically in a
/// session's cap-token allowlist once this tool is registered (every
/// registered tool's `required_capabilities` labels are folded into the
/// session token — see `ardur-server`'s `tool_allowlist_for_runtime`), so no
/// separate grant step is needed to make delegation reachable.
const DELEGATE_CAPABILITY: &str = "multi_agent_delegate";
const DELEGATE_TOOL_ID: &str = "delegate_task";

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
pub struct DelegateTaskTool<D: DenyList + Clone + Send + Sync + 'static = HashSetDenyList> {
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
    /// Admission gate for the blueprint's `max_concurrency` default: at most
    /// this many `delegate_task` calls may be in flight (spawned but not yet
    /// terminated) at once across every caller sharing this tool instance.
    /// `FailFast` posture — a call beyond the ceiling is denied immediately
    /// rather than queued, matching the blueprint's default
    /// `ConcurrencyOverflowPosture`.
    concurrency: Arc<Semaphore>,
    max_concurrency: usize,
    /// The shared revocation deny-list every spawned child's verifier
    /// consults (gh#361). Cloning shares the underlying set, so a caller's
    /// token revoked through the server's runtime handle stops that caller's
    /// live delegations at their next turn. The plain constructors use a
    /// private in-memory list and do NOT see server-side revocations; only
    /// [`with_deny_list`](Self::with_deny_list) wires a shared one.
    deny: D,
    /// Real provider for supervised children (D1). `None` only when the
    /// legacy env-seam constructor could not build any backend; `invoke` then
    /// fails closed with a typed error instead of panicking during tool
    /// registration (a stubbed or credential-less server must still boot).
    provider: Option<Arc<dyn Provider + Send + Sync>>,
    /// The model delegated requests are pinned to. Real backends serialize
    /// the request's model id directly (Messages API `model`, CLI `--model`),
    /// so this must be the model the injected provider was built for, or one
    /// it explicitly serves.
    model: ModelId,
    /// Optional shared parent ledger (D1 hardening): when present, every
    /// delegation reserves from THIS budget, so two concurrent children
    /// cannot each spend the full allowance of an isolated per-call budget.
    /// Production wiring binds the session's real budget here (E4.3
    /// integration debt); without it each call gets an isolated ledger sized
    /// to its own envelope.
    session_budget: Option<ParentBudget>,
    /// Terminal outcomes whose waiter was dropped before delivery (caller
    /// timeout/cancel). Retained so the paid work's accounting is never lost
    /// with the invoke future; embedders drain via
    /// [`drain_settlements`](Self::drain_settlements).
    settlements: Arc<std::sync::Mutex<Vec<Result<ChildOutcome, ChildError>>>>,
}

impl DelegateTaskTool<HashSetDenyList> {
    /// The id this tool registers under, available without type arguments.
    pub const ID: &'static str = DELEGATE_TOOL_ID;

    /// Build a `delegate_task` tool that attenuates and verifies against
    /// `root`, checking spawned children's turns against `audience`, with
    /// the default concurrency ceiling ([`DEFAULT_MAX_CONCURRENCY`]).
    ///
    /// D1: requires an explicit provider (Arc<dyn Provider>) for the real
    /// supervised child. Use `with_provider` or pass from `ardur_provider_selector::from_env`.
    /// The old echo path is replaced; default seam uses from_env (integration debt
    /// noted for server registration sites).
    #[must_use]
    pub fn new(root: PublicKey, audience: impl Into<String>) -> Self {
        // Env seam for legacy call sites. NEVER panics: when no backend can
        // be built (stubbed/embedded server, credentials unset) the tool
        // registers anyway and fails closed with a typed error at invoke
        // time, so one missing provider cannot abort the whole registry.
        // Server registration should migrate to `with_deny_list_and_provider`
        // so children share the parent's injected provider (E4.3 debt).
        let model = std::env::var("ARDUR_MODEL")
            .ok()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| "default".to_string());
        let provider: Option<Arc<dyn Provider + Send + Sync>> =
            ardur_provider_selector::from_env(ModelId::new(model.clone()))
                .ok()
                .map(|p| -> Arc<dyn Provider + Send + Sync> { p });
        let mut tool =
            Self::with_max_concurrency(root, audience, DEFAULT_MAX_CONCURRENCY, provider);
        tool.model = ModelId::new(model);
        tool
    }

    /// Build a `delegate_task` tool with an explicit concurrency ceiling in
    /// place of [`DEFAULT_MAX_CONCURRENCY`]. The deny list is private and
    /// in-memory; see [`with_deny_list`](Self::with_deny_list) for the shared
    /// variant. `provider` may be `None` (env seam) — see [`new`](Self::new).
    #[must_use]
    pub fn with_max_concurrency(
        root: PublicKey,
        audience: impl Into<String>,
        max_concurrency: usize,
        provider: Option<Arc<dyn Provider + Send + Sync>>,
    ) -> Self {
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
                    "outcome": { "type": "string", "enum": ["completed", "failed", "cancelled", "budget_exhausted", "round_limit_reached"] },
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
            concurrency: Arc::new(Semaphore::new(max_concurrency)),
            max_concurrency,
            deny: HashSetDenyList::new(),
            provider,
            model: ModelId::new("default"),
            session_budget: None,
            settlements: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// D1: explicit provider injection for real supervised children.
    #[must_use]
    pub fn with_provider(
        root: PublicKey,
        audience: impl Into<String>,
        provider: Arc<dyn Provider + Send + Sync>,
    ) -> Self {
        Self::with_max_concurrency(root, audience, DEFAULT_MAX_CONCURRENCY, Some(provider))
    }
}

impl<D: DenyList + Clone + Send + Sync + 'static> DelegateTaskTool<D> {
    /// Build a `delegate_task` tool whose spawned children consult the shared
    /// deny-list `deny` on every turn (gh#361): revoking a caller's token
    /// through the handle the server handed the fused runtime stops that
    /// caller's live delegations at their next turn.
    #[must_use]
    pub fn with_deny_list(root: PublicKey, audience: impl Into<String>, deny: D) -> Self {
        let base = DelegateTaskTool::new(root, audience);
        Self {
            schema: base.schema,
            capabilities: base.capabilities,
            root: base.root,
            audience: base.audience,
            concurrency: base.concurrency,
            max_concurrency: base.max_concurrency,
            deny,
            provider: base.provider,
            model: base.model,
            session_budget: base.session_budget,
            settlements: base.settlements,
        }
    }

    /// [`with_deny_list`](Self::with_deny_list) plus an explicitly injected
    /// provider (D1 hardening): the server retains the SAME configured
    /// provider its own runtime was booted with, instead of the tool silently
    /// rebuilding a possibly different backend from the environment — or
    /// panicking when the environment names none.
    #[must_use]
    pub fn with_deny_list_and_provider(
        root: PublicKey,
        audience: impl Into<String>,
        deny: D,
        provider: Arc<dyn Provider + Send + Sync>,
    ) -> Self {
        let mut tool = Self::with_deny_list(root, audience, deny);
        tool.provider = Some(provider);
        tool
    }

    /// Pin delegated requests to `model`. Must be the model the injected
    /// provider was built for, or one it explicitly serves — real backends
    /// serialize this id directly.
    #[must_use]
    pub fn with_child_model(mut self, model: ModelId) -> Self {
        self.model = model;
        self
    }

    /// Reserve every delegation from one shared parent ledger (D1 hardening).
    /// Without this each call reserves from an isolated per-invocation budget
    /// sized to its own envelope, which can never be refused and is not shared
    /// with sibling delegations — two concurrent children could each spend the
    /// full allowance. Production wiring binds the session's real budget here
    /// (E4.3 integration debt).
    #[must_use]
    pub fn with_session_budget(mut self, budget: ParentBudget) -> Self {
        self.session_budget = Some(budget);
        self
    }

    /// Drain terminal child outcomes whose waiter was dropped before the
    /// result could be delivered (caller timeout/cancel). The paid work's
    /// accounting is retained here — independent of any invoke future — until
    /// the embedding runtime folds it into its own settlement path
    /// (server-side drain wiring is E4.3 integration debt).
    pub fn drain_settlements(&self) -> Vec<Result<ChildOutcome, ChildError>> {
        let mut guard = self
            .settlements
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::take(&mut *guard)
    }
}

#[async_trait]
impl<D: DenyList + Clone + Send + Sync + 'static> Tool for DelegateTaskTool<D> {
    fn id(&self) -> ToolId {
        ToolId::new(DELEGATE_TOOL_ID)
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
        // The JSON schema's `minimum: 1` is not runtime validation: a zero
        // budget would pass `can_afford` on a zero projection and dispatch a
        // real provider request that bills before anyone notices. Reject it
        // before attenuation or spawning.
        if matches!(args.max_cost_cents, Some(0)) {
            return Err(ToolError::InvalidArgs(
                "`max_cost_cents` must be >= 1: a zero budget cannot authorize any round"
                    .to_string(),
            ));
        }

        // Fail closed when the env-seam constructor found no backend: typed
        // error here, never a registration-time panic.
        let provider = self.provider.clone().ok_or_else(|| {
            ToolError::Internal(anyhow::anyhow!(
                "delegate_task has no provider: none was injected and the environment built none \
                 (set ARDUR_PROVIDER + its credentials, or construct with_provider / \
                 with_deny_list_and_provider)"
            ))
        })?;

        // Acquire permit up front (fail-fast). The permit is moved into the
        // driver task so it is held for the *actual* worker lifetime, not the
        // lifetime of this invoke future. This satisfies the permit-lifetime
        // guard.
        let permit =
            self.concurrency
                .clone()
                .try_acquire_owned()
                .map_err(|_| ToolError::Denied {
                    reason: format!(
                        "delegate_task concurrency ceiling ({}) reached; wait for an in-flight \
                     delegation to complete before spawning another",
                        self.max_concurrency
                    ),
                })?;

        let parent_token =
            CapToken::from_base64(&ctx.cap_token.0, &self.root).map_err(cap_token_denied)?;

        // Attenuate the parent token for the child: restrict to chat.submit
        // (recursion floor) and carve the budget. The TokenAuthority below
        // will re-verify audience/tool/budget/expiry/revocation at every
        // action boundary.
        let attenuator = BiscuitCapTokenAttenuator;
        let mut child_token = attenuator
            .attenuate(
                &parent_token,
                AttenuationRule::RestrictTools(vec![CHAT_SUBMIT_TOOL.to_string()]).into(),
            )
            .map_err(|e| {
                ToolError::Internal(anyhow::anyhow!("child token attenuation failed: {e}"))
            })?;
        let child_max = args.max_cost_cents.unwrap_or(DEFAULT_MAX_COST_CENTS) as u64;
        child_token = attenuator
            .attenuate(
                &child_token,
                AttenuationRule::ReduceBudget(child_max).into(),
            )
            .map_err(|e| ToolError::Internal(anyhow::anyhow!("child budget carve failed: {e}")))?;

        // Reserve from the shared parent ledger when one is bound (sibling
        // delegations then compete for one real allowance and this call can
        // legitimately be refused); otherwise from a per-invocation ledger
        // sized to this envelope (documented limitation until E4.3 binds the
        // session budget). Refusal stays typed.
        let ledger = self
            .session_budget
            .clone()
            .unwrap_or_else(|| ParentBudget::new(child_max));
        let reservation = ledger.reserve(child_max).map_err(map_child_error_to_tool)?;

        let authority = TokenAuthority {
            root: self.root,
            audience: self.audience.clone(),
            tool: "chat.submit".to_string(),
            now_unix: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            }),
        };

        let sup = ChildSupervisor::with_authority(
            Arc::clone(&provider),
            Box::new(self.deny.clone()),
            authority,
        );

        let spec = ChildSpec {
            prompt: args.goal.clone(),
            token: child_token,
            reservation,
            model: self.model.clone(),
            max_rounds: 20,
            envelope: CostEnvelope {
                // Per-ROUND admission projection, estimated from the rate
                // card — the lifetime ceiling lives in the reservation above.
                // Using the lifetime figure here would exhaust the child
                // after its first paid round.
                cents_max: per_round_estimate_cents(&*provider, 4096, child_max as u32),
                tokens_out_max: 4096,
                ..CostEnvelope::default()
            },
        };

        let child_handle = sup.spawn(spec).await.map_err(map_child_error_to_tool)?;

        // Retain the supervisor/child driver OUTSIDE the returned future from
        // invoke. Spawn a driver task that holds the permit and the child
        // handle (so dropping the invoke future does not detach the worker or
        // release the permit early). The driver sends the terminal outcome
        // over a oneshot; if the receiver is dropped the driver still runs to
        // completion and settles.
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<ChildOutcome, ChildError>>();
        let settlements = Arc::clone(&self.settlements);
        tokio::spawn(async move {
            let outcome = child_handle.join().await;
            // Release the permit only after real termination.
            drop(permit);
            // Deliver to the waiter if it is still listening. When the caller
            // timed out or cancelled (receiver dropped), retain the terminal
            // outcome instead of silently discarding it — the provider's
            // actual cost must survive for settlement even though no
            // ToolOutput will ever carry it (drain_settlements).
            if let Err(undelivered) = tx.send(outcome) {
                let mut guard = settlements
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.push(undelivered);
            }
        });

        let child_outcome = rx
            .await
            .map_err(|_| ToolError::Internal(anyhow::anyhow!("delegate child driver lost")))?
            .map_err(map_child_error_to_tool)?;

        let label = args.task_name.unwrap_or_else(|| "delegate".to_string());
        let agent_id = AgentId::new(format!("{label}-{}", Uuid::new_v4()));
        child_outcome_to_tool_result(child_outcome, agent_id, ReceiptId(ctx.invocation_id.0))
    }

    fn required_capabilities(&self) -> &[Capability] {
        &self.capabilities
    }
}

/// Estimate ONE round's cost from the provider's rate card, so the admission
/// projection is a per-round figure rather than the child's whole lifetime
/// budget — projecting the lifetime amount every round exhausts the
/// reservation after a single paid round even when many smaller rounds would
/// fit. After the first billed round the supervisor's `worst_round_cents`
/// takes over, so a rough card estimate is sufficient. Falls back to the
/// lifetime figure when the card prices nothing (zero-price test mocks and
/// backends), preserving the pre-hardening projection rather than inventing a
/// price.
fn per_round_estimate_cents(
    provider: &(dyn Provider + Send + Sync),
    tokens_out_max: u32,
    lifetime_cents: u32,
) -> u32 {
    let card = provider.rate_card();
    let k_tokens = f64::from(tokens_out_max) / 1000.0;
    let estimate = card.cents_per_request
        + card.cents_per_1k_input * k_tokens
        + card.cents_per_1k_output * k_tokens;
    if estimate.is_finite() && estimate >= 1.0 {
        (estimate.ceil() as u32).max(1).min(lifetime_cents)
    } else {
        lifetime_cents
    }
}

/// Fold a terminal [`ChildOutcome`] into the tool result, preserving the
/// taxonomy end to end: revocation and mid-flight authority failures stay
/// typed authorization errors (with the settled actuals in the reason), while
/// budget exhaustion, cancellation, round-limit and provider faults remain
/// distinguishable terminal states in the output — never one collapsed
/// "failed" with a generic receipt reason.
fn child_outcome_to_tool_result(
    outcome: ChildOutcome,
    agent_id: AgentId,
    parent_receipt_id: ReceiptId,
) -> Result<ToolOutput, ToolError> {
    let cost_from_child = outcome.cost();
    let rounds = outcome.rounds();
    let receipt_cost = ReceiptCostTuple {
        tokens_in: cost_from_child.tokens_in,
        tokens_out: cost_from_child.tokens_out,
        cents: cost_from_child.cents,
        wall_ms: cost_from_child.wall_ms,
        attention_score: cost_from_child.attention_score,
    };
    let (outcome_label, response_text, reason) = match outcome {
        ChildOutcome::Completed { text, .. } => ("completed", text, TerminationReason::Completed),
        ChildOutcome::Failed { reason, .. } => (
            "failed",
            String::new(),
            TerminationReason::ErrorOccurred(reason),
        ),
        ChildOutcome::Cancelled { .. } => (
            "cancelled",
            String::new(),
            TerminationReason::ErrorOccurred("cancelled by parent before completion".into()),
        ),
        ChildOutcome::BudgetExhausted { .. } => (
            "budget_exhausted",
            String::new(),
            TerminationReason::BudgetExhausted,
        ),
        ChildOutcome::RoundLimitReached { .. } => (
            "round_limit_reached",
            String::new(),
            TerminationReason::ErrorOccurred("round limit reached without a final answer".into()),
        ),
        ChildOutcome::Revoked { .. } => {
            return Err(ToolError::CapTokenDenied {
                reason: format!(
                    "child token revoked mid-flight after {rounds} round(s); settled {}c actuals",
                    receipt_cost.cents
                ),
            });
        }
        ChildOutcome::Unauthorized { reason, .. } => {
            return Err(ToolError::CapTokenDenied {
                reason: format!(
                    "child authority failed mid-flight after {rounds} round(s): {reason}; settled {}c actuals",
                    receipt_cost.cents
                ),
            });
        }
    };

    // Synthesize a minimal termination receipt (the real receipt chain
    // will be wired in later slices; for D1 the cost and verb are truthful).
    let receipt = TerminationReceipt {
        receipt_id: ReceiptId(Uuid::new_v4()),
        agent_id: agent_id.clone(),
        reason,
        total_cost: receipt_cost,
        terminated_at: ardur_receipt::UnixTsMillis(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        ),
        parent_receipt_id,
    };

    let outcome = DelegationOutcome {
        child_agent_id: agent_id,
        outcome_label,
        response_text,
        cost_used: receipt_cost,
        receipt,
    };

    Ok(outcome.into_tool_output())
}

fn map_child_error_to_tool(e: ChildError) -> ToolError {
    match e {
        ChildError::ReservationTooLarge {
            requested,
            available,
        } => ToolError::Denied {
            reason: format!("child reservation {requested} exceeds available {available}"),
        },
        ChildError::AlreadyRevoked => ToolError::CapTokenDenied {
            reason: "child token already revoked at spawn".into(),
        },
        ChildError::EmptyPrompt => ToolError::InvalidArgs("empty goal".into()),
        ChildError::WorkerLost(msg) => ToolError::Internal(anyhow::anyhow!(msg)),
        ChildError::Unauthorized { reason } => ToolError::CapTokenDenied { reason },
    }
}

// Re-use the legacy structs for output compatibility (D1 does not change the
// ToolOutput contract beyond widening `outcome` to the truthful terminal labels).
struct DelegationOutcome {
    child_agent_id: AgentId,
    outcome_label: &'static str,
    response_text: String,
    cost_used: ReceiptCostTuple,
    receipt: TerminationReceipt,
}

impl DelegationOutcome {
    fn into_tool_output(self) -> ToolOutput {
        let content = json!({
            "child_agent_id": self.child_agent_id.0,
            "outcome": self.outcome_label,
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

fn termination_reason_label(reason: &TerminationReason) -> &'static str {
    match reason {
        TerminationReason::Completed => "completed",
        TerminationReason::BudgetExhausted => "budget_exhausted",
        TerminationReason::TimedOut { .. } => "timed_out",
        TerminationReason::Cancelled { .. } => "cancelled",
        TerminationReason::ErrorOccurred(_) => "error",
    }
}

fn cap_token_denied(err: CapTokenError) -> ToolError {
    ToolError::CapTokenDenied {
        reason: format!("parent cap-token invalid or expired: {err}"),
    }
}

#[cfg(test)]
mod denial_tests {
    use super::*;

    /// The ChildError -> ToolError mapping must stay typed per variant; a
    /// future remap that flattens these into one class is caught here.
    #[test]
    fn child_error_maps_to_typed_tool_errors() {
        let reservation = map_child_error_to_tool(ChildError::ReservationTooLarge {
            requested: 10,
            available: 4,
        });
        assert!(
            matches!(reservation, ToolError::Denied { .. }),
            "reservation refusal must be a typed denial, got {reservation:?}"
        );
        let revoked = map_child_error_to_tool(ChildError::AlreadyRevoked);
        assert!(
            matches!(revoked, ToolError::CapTokenDenied { .. }),
            "pre-spawn revocation must be a cap-token denial, got {revoked:?}"
        );
        let empty = map_child_error_to_tool(ChildError::EmptyPrompt);
        assert!(
            matches!(empty, ToolError::InvalidArgs(_)),
            "empty prompt must be invalid args, got {empty:?}"
        );
        let lost = map_child_error_to_tool(ChildError::WorkerLost("panicked".into()));
        assert!(
            matches!(lost, ToolError::Internal(_)),
            "worker loss must be internal, got {lost:?}"
        );
        let unauthorized = map_child_error_to_tool(ChildError::Unauthorized {
            reason: "expired".into(),
        });
        assert!(
            matches!(unauthorized, ToolError::CapTokenDenied { .. }),
            "pre-spawn authority failure must be a cap-token denial, got {unauthorized:?}"
        );
    }

    /// Mid-flight revocation must surface as a typed authorization error
    /// carrying the settled actuals — never as a generic "failed" output that
    /// auditors cannot distinguish from a provider fault.
    #[test]
    fn revoked_outcome_surfaces_typed_auth_error_with_settled_cost() {
        let outcome = ChildOutcome::Revoked {
            rounds: 2,
            cost: CostTuple {
                cents: 42,
                ..CostTuple::default()
            },
        };
        let err =
            child_outcome_to_tool_result(outcome, AgentId::new("t"), ReceiptId(Uuid::new_v4()))
                .expect_err("revocation must not collapse into a generic failed output");
        match err {
            ToolError::CapTokenDenied { reason } => {
                assert!(
                    reason.contains("42"),
                    "settled actuals must ride the denial reason, got: {reason}"
                );
            }
            other => panic!("revocation must be CapTokenDenied, got {other:?}"),
        }
    }

    /// Budget exhaustion is a normal terminal state with its own label and a
    /// BudgetExhausted receipt reason — distinct from a provider fault.
    #[test]
    fn budget_exhaustion_is_distinct_from_provider_fault() {
        let budget = child_outcome_to_tool_result(
            ChildOutcome::BudgetExhausted {
                rounds: 3,
                cost: CostTuple {
                    cents: 10,
                    ..CostTuple::default()
                },
            },
            AgentId::new("t"),
            ReceiptId(Uuid::new_v4()),
        )
        .expect("budget exhaustion is a truthful terminal output, not an error");
        assert_eq!(budget.content["outcome"], "budget_exhausted");
        assert_eq!(budget.receipt_data["reason"], "budget_exhausted");

        let fault = child_outcome_to_tool_result(
            ChildOutcome::Failed {
                reason: "upstream 500".into(),
                rounds: 1,
                cost: CostTuple::default(),
            },
            AgentId::new("t"),
            ReceiptId(Uuid::new_v4()),
        )
        .expect("provider fault is a failed output, not a tool error");
        assert_eq!(fault.content["outcome"], "failed");
        assert_eq!(fault.receipt_data["reason"], "error");
    }
}
