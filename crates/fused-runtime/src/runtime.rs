//! [`FusedRuntime`] — the fused [`ChatRuntime`] and its multi-stage [`submit`]
//! pipeline (cap-token → cedar → cost-gate → pre-submit hooks → injection-defense
//! → provider → receipt → post-receipt hooks → finalize → memory → journal). See
//! the crate root for the full stage list and the Option-B rationale.
//!
//! [`submit`]: FusedRuntime::submit

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ardur_cap_token::{
    BiscuitCapTokenVerifier, CapToken, CapTokenError, CapTokenVerifier, PublicKey, RequiredCaveats,
    VerifiedClaims,
};
use ardur_cedar_policy::{
    ActionRef, CedarPolicyBundle, Decision, EvaluationContext, PolicyBundle, PrincipalRef,
    ResourceRef,
};
use ardur_cost_gate::{
    AdmissionError, AdmissionRequest, Clock, CostAdmissionGate, CostEnvelope,
    CostTuple as GateCostTuple, HolderId as GateHolderId, InMemoryCostAdmissionGate,
    ModelId as GateModelId, ProviderId as GateProviderId, Reservation, Sha256Digest as GateSha256,
    TokenId as GateTokenId,
};
use ardur_injection_defense::{ContentSource, FilterRegistry, ScannableContent, Verdict};
use ardur_lifecycle_hooks::{
    ErrorCtx, HookError, HookRegistry, LifecyclePhase, PostReceiptCtx, PreSubmitCtx,
    PreSubmitOutcome, RevokeCtx,
};
use ardur_memory::{HolderId as MemoryHolderId, MemoryCard, MemoryControlPlane, MemoryRuntime};
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, ModelId, Provider, ProviderError,
    StreamEvent, ToolDef, Usage,
};
use ardur_receipt::{
    Es256SigningKey, ReceiptBody, ReceiptSigner, Sha256Digest, ToolCallReceipt, VerbObject,
};
use ardur_runtime::{
    CapTokenRef, ChatMessage, ChatRuntime, CostTuple as RuntimeCostTuple, ReceiptId, Role,
    RuntimeError, SessionId, SubmitRequest, SubmitResult, ToolCall,
};
use ardur_session_journals::{JournalEntry, SessionJournal};
use ardur_tool_registry::{Capability, InvocationId, ToolContext, ToolError, ToolId, ToolRegistry};
use parking_lot::Mutex;

use crate::receipts::{PersistedReceipt, load_persisted_chain};
use crate::reconcile::{
    ReconciliationAction, ReconciliationError, ReconciliationReport, ReconciliationStrategy,
};
use crate::shared::{SharedBudget, SharedDenyList};
use crate::streaming::{FusedEvent, StageKind};
use futures::{Stream, StreamExt as _};

/// The receipt verb minted for a completed turn (`verb.object.state.vN`).
pub(crate) const COMPLETION_VERB: &str = "llm.completion.minted.v1";

/// The `filter_id` reported in [`RuntimeError::InjectionBlocked`] when stage 4.5
/// blocks. The registry aggregates many filters into one combined verdict (and a
/// [`CombinedScanResult`](ardur_injection_defense::CombinedScanResult) does not
/// retain which member filter blocked), so the stage names itself; the matched
/// signatures live in the error's `reason` and `flags`.
const INJECTION_FILTER_STAGE_ID: &str = "injection-defense";

/// Per-request overrides for [`FusedRuntime::submit_with_provisioning`].
///
/// Each field defaults to the builder/cap-token-derived value when `None`, so
/// the empty (`Default`) value reproduces the plain [`submit`](ChatRuntime::submit)
/// behaviour exactly. This is what lets one boot-time runtime serve a
/// multi-tenant gateway: per turn it can fund the requesting user's budget,
/// verify against that user's tenant audience, and (rarely) redirect the budget
/// holder.
#[derive(Clone, Debug, Default)]
pub struct PerRequestProvisioning {
    /// Budget to provision for the turn's subject *before* admission. `None`
    /// uses the subject's existing balance (and admission fails with
    /// [`RuntimeError::CostCeilingExceeded`] if it has none). When `Some`, it is
    /// merged **additively** onto the existing balance — a per-turn top-up
    /// accumulates rather than discarding unspent budget — and a merge that
    /// breaches the gate's configured per-subject cap surfaces as
    /// [`RuntimeError::ProvisioningFailed`].
    pub budget: Option<GateCostTuple>,
    /// The audience the cap-token is verified against for this turn. `None` uses
    /// the builder default; `Some` lets a single runtime accept cap-tokens
    /// scoped to different tenant audiences.
    pub audience: Option<String>,
    /// The budget-holder subject the turn spends against. `None` derives it from
    /// the verified cap-token subject (the normal path, so a turn cannot spend
    /// against a holder the cap did not prove); `Some` overrides it — rare, and
    /// intended for impersonation-test fixtures.
    pub subject: Option<GateHolderId>,
}

/// A [`ChatRuntime`] that fuses every Phase-1 substrate crate behind one
/// ARD-488: releases a turn's cost reservation when the turn future is dropped
/// before it settles (outer `tokio::time::timeout`, `select!` loss, client or
/// stream disconnect). `take_reservation` is idempotent (returns `None` if
/// `finalize`/`release` already removed the reservation), so this is a pure
/// backstop — the in-band finalize/release paths are unchanged.
struct ReservationCancelGuard {
    gate: Arc<InMemoryCostAdmissionGate<SharedBudget>>,
    budget: SharedBudget,
    reservation_id: uuid::Uuid,
}

impl ReservationCancelGuard {
    fn new(
        gate: Arc<InMemoryCostAdmissionGate<SharedBudget>>,
        budget: SharedBudget,
        reservation_id: uuid::Uuid,
    ) -> Self {
        Self {
            gate,
            budget,
            reservation_id,
        }
    }
}

impl Drop for ReservationCancelGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.gate.take_reservation(self.reservation_id) {
            let delta = ardur_cost_gate::CostDelta::full_credit(&handle.reserved);
            let _ = self.budget.refund_sync(handle, delta);
        }
    }
}

/// [`submit`](FusedRuntime::submit). Build it with
/// [`FusedRuntimeBuilder`](crate::FusedRuntimeBuilder).
pub struct FusedRuntime {
    pub(crate) cap_root: PublicKey,
    pub(crate) verifier: BiscuitCapTokenVerifier<SharedDenyList>,
    pub(crate) deny: SharedDenyList,
    pub(crate) audience: String,
    pub(crate) tool: String,
    pub(crate) cost_units: u64,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) policies: CedarPolicyBundle,
    pub(crate) principal_entity_type: String,
    pub(crate) action: ActionRef,
    pub(crate) cedar_attributes: serde_json::Value,
    pub(crate) provider: Arc<dyn Provider>,
    pub(crate) model: ModelId,
    pub(crate) max_tokens: u32,
    pub(crate) receipt_key: Es256SigningKey,
    pub(crate) verb: VerbObject,
    pub(crate) gate: Arc<InMemoryCostAdmissionGate<SharedBudget>>,
    pub(crate) budget: SharedBudget,
    pub(crate) gate_provider_id: GateProviderId,
    pub(crate) gate_model_id: GateModelId,
    pub(crate) envelope: CostEnvelope,
    pub(crate) registry: Arc<HookRegistry>,
    pub(crate) injection_filters: FilterRegistry,
    pub(crate) memory: Option<Arc<dyn MemoryRuntime + Send + Sync>>,
    pub(crate) journal: Option<Arc<dyn SessionJournal>>,
    pub(crate) chain_tail: Mutex<Option<Sha256Digest>>,
    pub(crate) receipt_log: Option<PathBuf>,
    pub(crate) reconciliation_strategy: ReconciliationStrategy,
    /// §6.0 — the tools the model may call, advertised to the provider and
    /// looked up to invoke. Empty (the builder default) means the loop runs once
    /// and tool-use responses surface as the final answer.
    pub(crate) tools: Arc<ToolRegistry>,
    /// §6.0 — the maximum number of provider iterations that may request tools
    /// before the turn aborts with [`RuntimeError::ToolLoopExhausted`].
    pub(crate) max_tool_iterations: u32,
    /// §6.0 — the per-tool-call deadline.
    pub(crate) tool_timeout: Duration,
}

impl FusedRuntime {
    /// The hook registry threaded through every turn.
    #[must_use]
    pub fn registry(&self) -> &Arc<HookRegistry> {
        &self.registry
    }

    /// The holder's remaining budget, or `None` if the holder was never
    /// provisioned. Reads the *same* ledger the cost gate reserves against, so a
    /// test can confirm no reservation was stranded.
    pub async fn remaining_budget(&self, holder: &GateHolderId) -> Option<GateCostTuple> {
        ardur_cost_gate::BudgetStore::current_balance(&self.budget, holder)
            .await
            .ok()
    }

    /// Verify `cap_token` for an operator/control-plane capability under this
    /// runtime's issuer root, audience, deny-list, clock, and cost units.
    ///
    /// This lets surfaces such as the CLI memory explorer reuse the exact
    /// cap-token verification substrate instead of manufacturing claims locally.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] when the token is missing, expired, revoked,
    /// malformed, or does not grant `tool`.
    pub fn verify_cap_token_for_tool(
        &self,
        cap_token: &CapTokenRef,
        tool: &str,
    ) -> Result<VerifiedClaims, RuntimeError> {
        self.stage_cap_token_for_tool(
            &SubmitRequest {
                messages: Vec::new(),
                cap_token: cap_token.clone(),
                session_id: SessionId::new(),
                requested_provider: None,
            },
            &PerRequestProvisioning::default(),
            self.clock.now_ms() / 1000,
            tool,
        )
    }

    /// Revoke a capability token mid-session: add its revocation ids to the
    /// shared deny-list (so the next turn carrying it fails at stage 1 with
    /// [`RuntimeError::CapDenied`]) and fire `on_revoke` across the registry.
    pub async fn revoke_cap_token(
        &self,
        session_id: SessionId,
        cap_token: CapTokenRef,
        revocation_reason: impl Into<String>,
    ) -> Result<Vec<HookError>, RuntimeError> {
        let token = CapToken::from_base64(&cap_token.0, &self.cap_root).map_err(|e| {
            RuntimeError::CapDenied {
                reason: format!("revoke: {e}"),
            }
        })?;
        self.deny.revoke_token(&token);
        let ctx = RevokeCtx {
            session_id,
            cap_token_id: &cap_token,
            revocation_reason: revocation_reason.into(),
        };
        Ok(self.registry.run_revoke(&ctx).await)
    }

    /// Fire `on_error` across the registry, swallowing hook-side errors (an
    /// error hook that itself fails must not mask the original failure).
    async fn fire_error(
        &self,
        session_id: SessionId,
        phase: LifecyclePhase,
        error: &(dyn std::error::Error + Send + Sync + '_),
    ) {
        let ctx = ErrorCtx {
            session_id,
            phase,
            error,
        };
        let _ = self.registry.run_error(&ctx).await;
    }

    /// Release a reservation on an error path — finalize it to zero actual cost
    /// so the gate refunds the entire hold. Best-effort: a failure here cannot
    /// be acted on (the turn is already aborting), and an un-refunded hold lapses
    /// on the gate's TTL regardless.
    async fn release(&self, reservation: Reservation) {
        let _ = self.gate.finalize(reservation, GateCostTuple::ZERO).await;
    }

    /// **Stage 4.5 (ARD-48).** Scan the outbound completion request's prompt
    /// through the injection-defense [`FilterRegistry`] and return the request to
    /// forward to the provider.
    ///
    /// - `Allow` → the request is returned unchanged.
    /// - `AllowWithSanitization` → the most-recent user message is rewritten to
    ///   the sanitized (redacted) text, so the provider sees the safe rewrite
    ///   while the raw prompt is preserved everywhere else (notably the journal,
    ///   which reads the original `req.messages`).
    /// - `Block` → returns [`RuntimeError::InjectionBlocked`]; the caller releases
    ///   the cost reservation and aborts before the provider is reached.
    ///
    /// An empty registry (the builder default) short-circuits to `Allow` so the
    /// stage is a true no-op unless the caller opts in via
    /// [`FusedRuntimeBuilder::with_injection_filters`](crate::FusedRuntimeBuilder::with_injection_filters).
    /// A scan that itself errors degrades to [`RuntimeError::Internal`] — a fail
    /// closed posture, since the prompt could not be cleared.
    ///
    /// Only the most-recent `User` message is scanned: earlier turns were scanned
    /// when they were submitted, and the system/assistant transcript is the
    /// runtime's own, not attacker-controlled inbound content. Tool outputs that
    /// re-enter as the next turn's input (`ContentSource::ToolReturn`) are scanned
    /// once tool-use lands — see the `TODO ARD-22` at the call site.
    async fn scan_outbound_request(
        &self,
        mut request: CompletionRequest,
    ) -> Result<CompletionRequest, RuntimeError> {
        if self.injection_filters.is_empty() {
            return Ok(request);
        }
        let Some(idx) = request
            .messages
            .iter()
            .rposition(|m| matches!(m.role, Role::User))
        else {
            return Ok(request);
        };
        let content = ScannableContent::UserMessage {
            text: request.messages[idx].content.clone(),
            source: ContentSource::Direct,
        };
        let scan = self
            .injection_filters
            .scan_all(&content)
            .await
            .map_err(|e| RuntimeError::Internal(anyhow::anyhow!("injection scan failed: {e}")))?;
        match scan.verdict {
            Verdict::Allow => Ok(request),
            Verdict::AllowWithSanitization { sanitized } => {
                request.messages[idx].content = sanitized;
                Ok(request)
            }
            Verdict::Block { reason } => Err(RuntimeError::injection_blocked(
                INJECTION_FILTER_STAGE_ID,
                reason,
                scan.flags,
            )),
        }
    }

    /// **§6.0.** The tools advertised to the provider this turn — one
    /// [`ToolDef`] per registered tool, projected from its registry schema. An
    /// empty registry yields an empty list, so the request is byte-identical to
    /// a pre-tool one and the loop settles on the first provider response.
    fn tool_defs(&self) -> Vec<ToolDef> {
        self.tools
            .list()
            .into_iter()
            .map(|t| {
                let schema = t.schema();
                ToolDef {
                    name: t.id().0,
                    description: schema.description.clone(),
                    input_schema: schema.input_schema.clone(),
                }
            })
            .collect()
    }

    /// **§6.0.** The ambient context a tool invocation runs against. The cwd is
    /// the process working directory and the env is empty — Phase-1 tools (echo,
    /// health-check, remote MCP) do not consult either; the cost budget is the
    /// per-turn envelope's cents ceiling.
    fn tool_context(&self, cap_token: &CapTokenRef, session_id: SessionId) -> ToolContext {
        ToolContext {
            cap_token: cap_token.clone(),
            session_id,
            invocation_id: InvocationId::new(),
            cwd: std::env::current_dir().unwrap_or_default(),
            env: HashMap::new(),
            cost_budget_cents: self.envelope.cents_max,
        }
    }

    /// **§6.0 (ARD-22).** Scan a tool's output through the injection-defense
    /// registry before it re-enters the transcript as the next provider call's
    /// input — a `ToolReturn`-sourced scan, since a tool (especially a remote
    /// MCP one) is attacker-influenced content, not the runtime's own.
    ///
    /// An empty registry short-circuits to allow. A `Block` verdict surfaces as
    /// [`RuntimeError::InjectionBlocked`]; `AllowWithSanitization` is treated as
    /// allow here (the structured JSON output is not rewritten in place in P1).
    async fn scan_tool_output(
        &self,
        tool_name: &str,
        output: &serde_json::Value,
    ) -> Result<(), RuntimeError> {
        if self.injection_filters.is_empty() {
            return Ok(());
        }
        let content = ScannableContent::ToolOutput {
            tool_id: ToolId::new(tool_name),
            output: output.clone(),
        };
        let scan = self
            .injection_filters
            .scan_all(&content)
            .await
            .map_err(|e| {
                RuntimeError::Internal(anyhow::anyhow!("tool-output injection scan failed: {e}"))
            })?;
        match scan.verdict {
            Verdict::Allow | Verdict::AllowWithSanitization { .. } => Ok(()),
            Verdict::Block { reason } => Err(RuntimeError::injection_blocked(
                INJECTION_FILTER_STAGE_ID,
                reason,
                scan.flags,
            )),
        }
    }

    /// **Stage 1.** Parse + verify the request's cap-token against the root, the
    /// audience (the per-request override if supplied, else the builder default),
    /// the tool, and the deny-list — returning the verified claims. Fires no
    /// hooks: both [`submit`](Self::submit_inner) and
    /// [`stream`](Self::stream_inner) call this and bracket it with their own
    /// error reporting (a `fire_error` call / a `StageEnd` event), so the
    /// verification rule lives in exactly one place.
    fn stage_cap_token(
        &self,
        req: &SubmitRequest,
        provisioning: &PerRequestProvisioning,
        now_unix: u64,
    ) -> Result<VerifiedClaims, RuntimeError> {
        self.stage_cap_token_for_tool(req, provisioning, now_unix, &self.tool)
    }

    fn stage_cap_token_for_tool(
        &self,
        req: &SubmitRequest,
        provisioning: &PerRequestProvisioning,
        now_unix: u64,
        tool: &str,
    ) -> Result<VerifiedClaims, RuntimeError> {
        if req.cap_token.0.is_empty() {
            return Err(RuntimeError::CapTokenMissing);
        }
        let token = CapToken::from_base64(&req.cap_token.0, &self.cap_root).map_err(|e| {
            RuntimeError::CapDenied {
                reason: e.to_string(),
            }
        })?;
        let audience = provisioning
            .audience
            .clone()
            .unwrap_or_else(|| self.audience.clone());
        self.verifier
            .verify(
                &token,
                &self.cap_root,
                &RequiredCaveats {
                    now_unix,
                    audience,
                    tool: tool.to_string(),
                    cost: self.cost_units,
                },
            )
            .map_err(|e| match e {
                CapTokenError::Expired => RuntimeError::CapTokenExpired,
                other => RuntimeError::CapDenied {
                    reason: other.to_string(),
                },
            })
    }

    /// **Stage 2.** Authorize the turn against the Cedar bundle. The principal is
    /// *derived* from the verified cap-token subject (never caller-asserted) and
    /// the resource from the session; the cap claims ride as resource attributes.
    /// Fires no hooks (see [`stage_cap_token`](Self::stage_cap_token)).
    fn stage_cedar(
        &self,
        session_id: SessionId,
        claims: &VerifiedClaims,
    ) -> Result<(), RuntimeError> {
        self.stage_cedar_with_action(session_id, claims, self.action.clone())
    }

    fn stage_cedar_with_action(
        &self,
        session_id: SessionId,
        claims: &VerifiedClaims,
        action: ActionRef,
    ) -> Result<(), RuntimeError> {
        let principal = derive_principal(&self.principal_entity_type, claims);
        let resource = derive_resource(session_id);
        let attributes = cedar_attributes_from_claims(&self.cedar_attributes, claims);
        match self.policies.evaluate(&EvaluationContext {
            principal,
            action,
            resource,
            attributes,
        }) {
            Decision::Allow { .. } => Ok(()),
            Decision::Deny { reason, .. } => Err(RuntimeError::PolicyDenied { reason }),
            Decision::Indeterminate { reason } => Err(RuntimeError::PolicyDenied {
                reason: format!("indeterminate: {reason}"),
            }),
        }
    }

    fn authorize_tool_invocation(
        &self,
        req: &SubmitRequest,
        provisioning: &PerRequestProvisioning,
        session_id: SessionId,
        now_unix: u64,
        tool_name: &str,
    ) -> Result<(), RuntimeError> {
        let claims = self.stage_cap_token_for_tool(req, provisioning, now_unix, tool_name)?;
        self.stage_cedar_with_action(
            session_id,
            &claims,
            ActionRef("Action::ToolInvoke".to_string()),
        )
    }

    /// **ARD-420.** Check the tool's declared [`Capability`]s against the
    /// verified cap-token claims before `invoke` runs. Each required capability
    /// (as a `cap.*` string via [`Capability::as_str`]) must appear in the
    /// cap-token's `tool_allowlist`; otherwise the call is denied with
    /// [`RuntimeError::CapDenied`] before the tool body executes.
    fn authorize_tool_capabilities(
        &self,
        req: &SubmitRequest,
        provisioning: &PerRequestProvisioning,
        now_unix: u64,
        tool_name: &str,
        required: &[Capability],
    ) -> Result<(), RuntimeError> {
        if required.is_empty() {
            return Ok(());
        }
        let claims = self.stage_cap_token_for_tool(req, provisioning, now_unix, tool_name)?;
        for cap in required {
            let label = cap.as_str();
            if !claims.tool_allowlist.iter().any(|t| t == &label) {
                return Err(RuntimeError::CapDenied {
                    reason: format!(
                        "tool `{tool_name}` requires capability `{label}` which is not granted by the cap-token"
                    ),
                });
            }
        }
        Ok(())
    }

    /// **Stage 3 (setup).** Resolve the budget holder (the verified subject
    /// unless the request overrides it), apply a per-request top-up if one was
    /// supplied, and bind the verified token to the holder. Returns the gate
    /// token id the per-round admission reserves under. The per-round `admit` /
    /// `finalize` happen inside each turn's loop. Fires no hooks.
    async fn stage_cost_setup(
        &self,
        claims: &VerifiedClaims,
        provisioning: &PerRequestProvisioning,
    ) -> Result<GateTokenId, RuntimeError> {
        let gate_token_id = GateTokenId(claims.token_id);
        let holder = provisioning
            .subject
            .clone()
            .unwrap_or_else(|| GateHolderId(claims.subject.0.clone()));
        if let Some(budget) = provisioning.budget {
            self.gate
                .provision_for(&holder, budget)
                .await
                .map_err(|e| RuntimeError::ProvisioningFailed {
                    subject: holder.0.clone(),
                    reason: e.to_string(),
                })?;
        }
        self.gate.bind_token(gate_token_id, holder);
        Ok(gate_token_id)
    }

    /// **Stage 4.** Run the pre-submit hooks over the initial request and return
    /// the request the turn loop starts from. A `Veto` is
    /// [`RuntimeError::VetoedByHook`] (no reservation is held yet, so nothing to
    /// release); a `Replace` swaps the request. Like the other stage helpers it
    /// fires no hooks of its own.
    async fn stage_pre_submit(
        &self,
        req: &SubmitRequest,
        tool_defs: Vec<ToolDef>,
    ) -> Result<CompletionRequest, RuntimeError> {
        let base_request =
            CompletionRequest::new(req.messages.clone(), self.model.clone(), self.max_tokens)
                .with_tools(tool_defs);
        let pre_ctx = PreSubmitCtx {
            session_id: req.session_id,
            request: &base_request,
            cap_token_id: &req.cap_token,
            attempt: 1,
        };
        match self.registry.run_pre_submit(&pre_ctx).await {
            PreSubmitOutcome::Continue => Ok(base_request),
            PreSubmitOutcome::Replaced { request } => Ok(request),
            PreSubmitOutcome::Vetoed { hook_id, reason } => Err(RuntimeError::VetoedByHook {
                hook_id: hook_id.to_string(),
                reason,
            }),
        }
    }

    /// Recall memories for the verified cap-token subject and inject them as a
    /// system context block before provider dispatch.
    ///
    /// This runs only after cap-token verification and Cedar authorization have
    /// succeeded, so memory reads inherit the same security gate as the turn. The
    /// search itself is subject-scoped; a backend bug that returns another
    /// workspace's record is filtered by the memory runtime before this formatter
    /// can see it.
    fn inject_recalled_memories(
        &self,
        mut request: CompletionRequest,
        claims: &VerifiedClaims,
    ) -> Result<CompletionRequest, RuntimeError> {
        let Some(memory) = &self.memory else {
            return Ok(request);
        };
        let Some(query) = request
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, Role::User))
            .map(|m| m.content.as_str())
        else {
            return Ok(request);
        };
        if query.trim().is_empty() {
            return Ok(request);
        }
        let subject = MemoryHolderId(claims.subject.0.clone());
        let hits = memory
            .search_scoped(&subject, query, 5)
            .map_err(|e| RuntimeError::Internal(anyhow::anyhow!("memory recall failed: {e}")))?;
        if hits.is_empty() {
            return Ok(request);
        }

        let mut block = format!(
            "Relevant memories (scoped to verified subject {}):\n",
            claims.subject.0
        );
        for rec in hits {
            let card = MemoryCard::from_record(&rec);
            let receipt = card
                .receipt_id
                .map(|r| r.0.to_string())
                .unwrap_or_else(|| "unreceipted".to_string());
            let source = card.source.unwrap_or_else(|| "unknown".to_string());
            let scope = card.scope.unwrap_or_else(|| card.subject.0.clone());
            let confidence = card
                .confidence
                .map(|c| format!("{c:.2}"))
                .unwrap_or_else(|| "unknown".to_string());
            block.push_str(&format!(
                "- id={} source={} scope={} confidence={} receipt={} valid_from={}: {}\n",
                card.record_id,
                source,
                scope,
                confidence,
                receipt,
                card.valid_from.0,
                memory_payload_text(&card.payload)
            ));
        }
        request.messages.insert(0, ChatMessage::system(block));
        Ok(request)
    }

    async fn commit_receipt_and_journal(
        &self,
        _session_id: SessionId,
        iteration: u32,
        req: &SubmitRequest,
        response: &CompletionResponse,
        signed: &ardur_receipt::SignedReceipt,
        now_ms: u64,
    ) -> Result<ReceiptBody, RuntimeError> {
        let receipt = signed.body().clone();
        let journal_start_len = match &self.journal {
            Some(journal) => Some(journal.len().await.map_err(|e| {
                RuntimeError::Internal(anyhow::anyhow!(
                    "journal length read failed before receipt commit: {e}"
                ))
            })?),
            None => None,
        };

        if let Some(journal) = &self.journal {
            if iteration == 1 {
                if let Some(prompt) = last_user_message(&req.messages) {
                    if let Err(e) = journal
                        .append(JournalEntry::UserMessage {
                            content: prompt.to_string(),
                            at: now_ms,
                        })
                        .await
                    {
                        return Err(RuntimeError::Internal(anyhow::anyhow!(
                            "journal user append failed before receipt commit: {e}"
                        )));
                    }
                }
            }
            if let Err(e) = journal
                .append(JournalEntry::AssistantMessage {
                    content: response.content.clone(),
                    at: now_ms,
                    receipt_id: ReceiptId(receipt.receipt_id),
                })
                .await
            {
                if let Some(start_len) = journal_start_len {
                    let _ = journal.truncate(start_len).await;
                }
                return Err(RuntimeError::Internal(anyhow::anyhow!(
                    "journal assistant append failed before receipt commit: {e}"
                )));
            }
        }

        if let Err(e) = self.persist_receipt(signed.jws_compact()) {
            if let (Some(journal), Some(start_len)) = (&self.journal, journal_start_len) {
                if let Err(rollback_err) = journal.truncate(start_len).await {
                    return Err(RuntimeError::Internal(anyhow::anyhow!(
                        "receipt persist failed after journal append: {e}; journal rollback failed: {rollback_err}"
                    )));
                }
            }
            return Err(RuntimeError::Internal(anyhow::anyhow!(
                "receipt persist failed after journal append: {e}"
            )));
        }
        *self.chain_tail.lock() = Some(Sha256Digest::of(signed.jws_compact().as_bytes()));
        Ok(receipt)
    }

    /// Append a signed receipt's compact JWS to the durable receipt log
    /// (one line, fsynced), if a log path is configured.
    fn persist_receipt(&self, jws_compact: &str) -> std::io::Result<()> {
        let Some(path) = &self.receipt_log else {
            return Ok(());
        };
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{jws_compact}")?;
        // fsync the line so the chain survives a crash — the same durability
        // contract the session journal makes for its entries.
        file.sync_all()
    }

    /// The reconciliation strategy this runtime applies in
    /// [`reconcile_receipts`](Self::reconcile_receipts).
    #[must_use]
    pub fn reconciliation_strategy(&self) -> ReconciliationStrategy {
        self.reconciliation_strategy
    }

    /// **ARD-17.** Sweep the receipt log against the session journal to detect —
    /// and, unless `dry_run`, heal — *orphan receipts*: receipts durable in the
    /// chain that no journal `AssistantMessage` accounts for, the residue of a
    /// crash in the stage-6→10 window (see [`crate::reconcile`]).
    ///
    /// Intended as a **boot step**: a fresh runtime built over a crashed
    /// runtime's on-disk paths calls this once before serving turns, so the
    /// journal and the receipt log agree again. [`FusedRuntimeBuilder::build`]
    /// stays synchronous (it cannot drive the async journal), so this is exposed
    /// explicitly; [`FusedRuntimeBuilder::build_reconciled`] folds the two into
    /// one call for callers that want the boot default.
    ///
    /// Reconciliation is a no-op (with an empty report) unless **both** a
    /// receipt log and a journal are configured — neither store alone can have an
    /// orphan. It is idempotent: a second pass after a successful
    /// [`AppendSyntheticJournal`](ReconciliationStrategy::AppendSyntheticJournal)
    /// recovery finds the once-orphaned receipt now journaled, and does nothing.
    ///
    /// # Errors
    ///
    /// - [`ReconciliationError::ReceiptChain`] if the receipt log cannot be read.
    /// - [`ReconciliationError::Journal`] if the journal cannot be replayed or
    ///   (under [`AppendSyntheticJournal`](ReconciliationStrategy::AppendSyntheticJournal))
    ///   appended to.
    /// - [`ReconciliationError::Io`] if rewriting a truncated log fails.
    /// - [`ReconciliationError::Undecidable`] if
    ///   [`TruncateOrphans`](ReconciliationStrategy::TruncateOrphans) is asked to
    ///   drop a non-suffix orphan (one a later journaled receipt chains onto),
    ///   which would break the hash chain.
    pub async fn reconcile_receipts(
        &self,
        dry_run: bool,
    ) -> Result<ReconciliationReport, ReconciliationError> {
        // Both stores are required: an orphan is a receipt the journal fails to
        // account for, so with no journal (or no log) there is nothing to
        // reconcile. Report an empty, no-orphan sweep.
        let (Some(journal), Some(receipt_log)) = (&self.journal, &self.receipt_log) else {
            return Ok(ReconciliationReport {
                receipt_count: 0,
                journaled_receipt_count: 0,
                orphan_receipt_ids: Vec::new(),
                action: ReconciliationAction::NoOrphans,
                dry_run,
            });
        };

        let chain = load_persisted_chain(receipt_log)?;
        let session_id = *journal.session_id();
        let entries = journal.replay(session_id).await?;

        // The set of receipt ids the journal can vouch for: every receipt named
        // by an AssistantMessage (the entry the pipeline writes at stage 10 to
        // bind a turn's response to its receipt). A recovery entry appended by a
        // prior reconciliation is itself an AssistantMessage, so this is what
        // makes the sweep idempotent.
        let journaled: std::collections::HashSet<uuid::Uuid> = entries
            .iter()
            .filter_map(|e| match e {
                JournalEntry::AssistantMessage { receipt_id, .. } => Some(receipt_id.0),
                _ => None,
            })
            .collect();

        let orphan_indices: Vec<usize> = chain
            .iter()
            .enumerate()
            .filter(|(_, r)| !journaled.contains(&r.body.receipt_id))
            .map(|(i, _)| i)
            .collect();
        let orphan_receipt_ids: Vec<uuid::Uuid> = orphan_indices
            .iter()
            .map(|&i| chain[i].body.receipt_id)
            .collect();

        let mut report = ReconciliationReport {
            receipt_count: chain.len(),
            journaled_receipt_count: journaled.len(),
            orphan_receipt_ids,
            action: ReconciliationAction::NoOrphans,
            dry_run,
        };

        if orphan_indices.is_empty() {
            return Ok(report);
        }
        if dry_run || self.reconciliation_strategy == ReconciliationStrategy::IgnoreOrphans {
            report.action = ReconciliationAction::ReportedOnly;
            return Ok(report);
        }

        match self.reconciliation_strategy {
            ReconciliationStrategy::IgnoreOrphans => unreachable!("handled above"),
            ReconciliationStrategy::AppendSyntheticJournal => {
                // Heal the journal: one recovery AssistantMessage per orphan,
                // naming its receipt_id so the next sweep counts it as journaled.
                // The original assistant text is lost (it was never journaled),
                // so the content is an explicit recovery marker, not a fabricated
                // response.
                let now = self.clock.now_ms();
                for &i in &orphan_indices {
                    let rid = chain[i].body.receipt_id;
                    journal
                        .append(JournalEntry::AssistantMessage {
                            content: format!(
                                "[reconciled] recovered orphan receipt {rid}: its receipt was \
                                 durably minted (stage 6) but the process crashed before the \
                                 journal append (stage 10), so the original assistant content is \
                                 unrecoverable."
                            ),
                            at: now,
                            receipt_id: ReceiptId(rid),
                        })
                        .await?;
                }
                report.action = ReconciliationAction::AppendedSyntheticJournal {
                    count: orphan_indices.len(),
                };
            }
            ReconciliationStrategy::TruncateOrphans => {
                self.truncate_orphan_suffix(receipt_log, &chain, &orphan_indices)?;
                report.action = ReconciliationAction::TruncatedReceipts {
                    count: orphan_indices.len(),
                };
            }
        }
        Ok(report)
    }

    /// Truncate a contiguous orphan *suffix* from the receipt log, rewriting it
    /// to the retained prefix and resetting the in-memory chain tail so the next
    /// turn chains onto the new last receipt.
    ///
    /// The orphans must form the maximal tail `first..chain.len()`. If any
    /// orphan sits *before* a journaled receipt, removing it would break that
    /// receipt's `parent_hash` linkage — that is
    /// [`ReconciliationError::Undecidable`], not a silent partial truncation.
    fn truncate_orphan_suffix(
        &self,
        receipt_log: &std::path::Path,
        chain: &[PersistedReceipt],
        orphan_indices: &[usize],
    ) -> Result<(), ReconciliationError> {
        let first_orphan = orphan_indices[0];
        let is_contiguous_suffix = orphan_indices.iter().copied().eq(first_orphan..chain.len());
        if !is_contiguous_suffix {
            return Err(ReconciliationError::Undecidable {
                reason: format!(
                    "{} orphan(s) are not a contiguous tail of the {}-receipt chain — a later \
                     journaled receipt chains onto an orphan, so truncating would break the hash \
                     chain. Use AppendSyntheticJournal to recover these in place.",
                    orphan_indices.len(),
                    chain.len()
                ),
            });
        }

        // Rewrite the log to the retained prefix (one compact JWS per line), then
        // fsync — the same write-then-sync_all durability contract persist_receipt
        // makes. A full rewrite of a freshly-truncated boot-time log is cheap.
        let retained = &chain[..first_orphan];
        let mut body = String::new();
        for receipt in retained {
            body.push_str(&receipt.jws_compact);
            body.push('\n');
        }
        let tmp = receipt_log.with_extension("jsonl.reconcile-tmp");
        std::fs::write(&tmp, body.as_bytes()).map_err(ReconciliationError::Io)?;
        // fsync the temp file's contents before the rename so the truncation is
        // durable, then atomically replace the log.
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&tmp)
                .map_err(ReconciliationError::Io)?;
            f.flush().map_err(ReconciliationError::Io)?;
            f.sync_all().map_err(ReconciliationError::Io)?;
        }
        std::fs::rename(&tmp, receipt_log).map_err(ReconciliationError::Io)?;

        // Reset the in-memory chain tail to the new last receipt (or None if the
        // whole chain was orphaned), so the next turn chains correctly. build()
        // had seeded it from the now-removed orphan tail.
        *self.chain_tail.lock() = retained
            .last()
            .map(|r| Sha256Digest::of(r.jws_compact.as_bytes()));
        Ok(())
    }
}

impl ChatRuntime for FusedRuntime {
    async fn submit(&self, req: SubmitRequest) -> Result<SubmitResult, RuntimeError> {
        self.submit_inner(req, PerRequestProvisioning::default())
            .await
    }
}

impl FusedRuntime {
    /// Submit a turn with per-request provisioning overrides — the multi-tenant
    /// entry point. Unlike [`submit`](ChatRuntime::submit) (which uses the
    /// builder's fixed audience and only the budget a holder was provisioned at
    /// build time), this accepts a [`PerRequestProvisioning`] that can:
    ///
    /// - **top the turn's subject up** ([`PerRequestProvisioning::budget`]) so a
    ///   server can fund a per-user budget on the request itself — the merge is
    ///   additive, so a top-up accumulates rather than discarding unspent budget;
    /// - **override the audience** ([`PerRequestProvisioning::audience`]) the
    ///   cap-token is verified against, so one runtime can serve cap-tokens
    ///   scoped to different tenant audiences;
    /// - **override the budget-holder subject**
    ///   ([`PerRequestProvisioning::subject`]) the turn spends against (rare —
    ///   normally the subject is derived from the verified cap-token).
    ///
    /// All three default to the builder/cap-token-derived values when `None`, so
    /// `submit(req)` is exactly `submit_with_provisioning(req, Default::default())`.
    pub async fn submit_with_provisioning(
        &self,
        req: SubmitRequest,
        provisioning: PerRequestProvisioning,
    ) -> Result<SubmitResult, RuntimeError> {
        self.submit_inner(req, provisioning).await
    }

    async fn submit_inner(
        &self,
        req: SubmitRequest,
        provisioning: PerRequestProvisioning,
    ) -> Result<SubmitResult, RuntimeError> {
        let session_id = req.session_id;
        let now_ms = self.clock.now_ms();
        let now_unix = now_ms / 1000;

        // ---- 1. cap-token: parse + verify against the root, audience, tool,
        //         and deny-list.
        let claims = match self.stage_cap_token(&req, &provisioning, now_unix) {
            Ok(claims) => claims,
            Err(err) => {
                self.fire_error(session_id, LifecyclePhase::Submit, &err)
                    .await;
                return Err(err);
            }
        };

        // ---- 2. cedar-policy: authorize the turn against the verified subject.
        if let Err(err) = self.stage_cedar(session_id, &claims) {
            self.fire_error(session_id, LifecyclePhase::Submit, &err)
                .await;
            return Err(err);
        }

        // ---- 3. cost-gate setup (once): resolve the holder, top it up if the
        //         request carries a budget, and bind the token. The per-iteration
        //         `admit`/`finalize` happen inside the tool-call loop below.
        let gate_token_id = match self.stage_cost_setup(&claims, &provisioning).await {
            Ok(gate_token_id) => gate_token_id,
            Err(err) => {
                self.fire_error(session_id, LifecyclePhase::Submit, &err)
                    .await;
                return Err(err);
            }
        };

        // The tools advertised to the provider every iteration of this turn.
        let tool_defs = self.tool_defs();

        // ---- 4. pre-submit hooks (once, on the initial request). A veto aborts
        //         (no reservation is held yet, so no release); a replace swaps the
        //         request the loop starts from.
        let initial = match self.stage_pre_submit(&req, tool_defs.clone()).await {
            Ok(request) => request,
            Err(err) => return Err(err),
        };
        let initial = match self.inject_recalled_memories(initial, &claims) {
            Ok(request) => request,
            Err(err) => {
                self.fire_error(session_id, LifecyclePhase::MemoryWrite, &err)
                    .await;
                return Err(err);
            }
        };

        // The working transcript the loop grows with each tool round trip, plus
        // the request knobs (a hook may have rewritten temperature / stops) that
        // ride onto every iteration's request.
        let mut messages = initial.messages;
        let temperature = initial.temperature;
        let stop_sequences = initial.stop_sequences;
        let requested_cost_envelope = initial.requested_cost_envelope;

        // ---- 5–10 + tool execution: the tool-call loop. Each iteration runs the
        //          full per-call pipeline — injection scan, cost-gate admit,
        //          provider dispatch, tool execution (with the tool output scanned
        //          and the model's calls recorded on the receipt), receipt mint,
        //          finalize, memory, and journal — then either settles on a final
        //          answer (no tool calls), continues with the tool results folded
        //          back in, or aborts with `ToolLoopExhausted`.
        let mut iteration: u32 = 0;
        let mut total_cost = RuntimeCostTuple::default();

        let (receipt, final_content) = loop {
            iteration += 1;

            // Build this iteration's request from the current transcript + tools.
            let mut iter_request =
                CompletionRequest::new(messages.clone(), self.model.clone(), self.max_tokens);
            iter_request.temperature = temperature;
            iter_request.stop_sequences = stop_sequences.clone();
            iter_request.requested_cost_envelope = requested_cost_envelope;
            iter_request.tools = tool_defs.clone();

            // 4.5 injection-defense: scan the most recent user message (tool
            // outputs are scanned at the point they are produced, below). No
            // reservation is held yet, so a block needs no release.
            let iter_request = match self.scan_outbound_request(iter_request).await {
                Ok(request) => request,
                Err(err) => {
                    self.fire_error(session_id, LifecyclePhase::Submit, &err)
                        .await;
                    return Err(err);
                }
            };

            // 3'. cost-gate admit (per iteration).
            let request_digest =
                GateSha256::of(&serde_json::to_vec(&iter_request.messages).unwrap_or_default());
            let reservation = match self
                .gate
                .admit(AdmissionRequest {
                    cap_token_id: gate_token_id,
                    projected_envelope: self.envelope,
                    provider_id: self.gate_provider_id.clone(),
                    model_id: self.gate_model_id.clone(),
                    request_digest,
                })
                .await
            {
                Ok(reservation) => reservation,
                Err(e) => {
                    let err = map_admission_error(e);
                    self.fire_error(session_id, LifecyclePhase::Submit, &err)
                        .await;
                    return Err(err);
                }
            };

            // ARD-488: release the reservation if this turn is cancelled (future
            // dropped / outer timeout) before it settles.
            let _cancel_guard = ReservationCancelGuard::new(
                Arc::clone(&self.gate),
                self.budget.clone(),
                reservation.reservation_id,
            );

            // 5. provider dispatch.
            let response = match self.provider.complete(iter_request).await {
                Ok(response) => response,
                Err(provider_err) => {
                    self.release(reservation).await;
                    self.fire_error(session_id, LifecyclePhase::Provider, &provider_err)
                        .await;
                    return Err(map_provider_error(&provider_err));
                }
            };

            // The tool calls (if any) the model requested this round.
            let requested: Vec<ToolCall> = match &response.finish_reason {
                FinishReason::ToolUse(calls) => calls.clone(),
                _ => Vec::new(),
            };
            let wants_tools = !requested.is_empty();
            // The loop is bounded: once we have made `max_tool_iterations`
            // provider calls and the model still wants tools, we abort rather
            // than execute another round.
            let exhausted = wants_tools && iteration >= self.max_tool_iterations;

            // Tool execution: invoke each requested tool (unless we are aborting),
            // scan its output, record it on the receipt, and stage the result
            // message for the next iteration.
            let mut tool_receipts: Vec<ToolCallReceipt> = Vec::new();
            let mut tool_messages: Vec<ChatMessage> = Vec::new();
            let mut tool_cost = RuntimeCostTuple::default();
            if wants_tools && !exhausted {
                for call in &requested {
                    let Some(tool) = self.tools.get(&ToolId::new(&call.name)) else {
                        self.release(reservation).await;
                        let err = RuntimeError::UnknownTool {
                            tool: call.name.clone(),
                        };
                        self.fire_error(session_id, LifecyclePhase::Provider, &err)
                            .await;
                        return Err(err);
                    };
                    if let Err(err) = self.authorize_tool_invocation(
                        &req,
                        &provisioning,
                        session_id,
                        now_unix,
                        &call.name,
                    ) {
                        self.release(reservation).await;
                        self.fire_error(session_id, LifecyclePhase::Submit, &err)
                            .await;
                        return Err(err);
                    }
                    // ARD-420: enforce required_capabilities() against the
                    // cap-token's tool allowlist before the tool body runs.
                    if let Err(err) = self.authorize_tool_capabilities(
                        &req,
                        &provisioning,
                        now_unix,
                        &call.name,
                        tool.required_capabilities(),
                    ) {
                        self.release(reservation).await;
                        self.fire_error(session_id, LifecyclePhase::Submit, &err)
                            .await;
                        return Err(err);
                    }
                    let ctx = self.tool_context(&req.cap_token, session_id);
                    let output = match tokio::time::timeout(
                        self.tool_timeout,
                        tool.invoke(&ctx, call.arguments.clone()),
                    )
                    .await
                    {
                        Ok(Ok(output)) => output,
                        Ok(Err(tool_err)) => {
                            self.release(reservation).await;
                            let err = map_tool_error(tool_err, &call.name);
                            self.fire_error(session_id, LifecyclePhase::Provider, &err)
                                .await;
                            return Err(err);
                        }
                        Err(_elapsed) => {
                            self.release(reservation).await;
                            let err = RuntimeError::ToolTimeout {
                                tool: call.name.clone(),
                            };
                            self.fire_error(session_id, LifecyclePhase::Provider, &err)
                                .await;
                            return Err(err);
                        }
                    };

                    // Scan the tool output before it re-enters the context.
                    if let Err(err) = self.scan_tool_output(&call.name, &output.content).await {
                        self.release(reservation).await;
                        self.fire_error(session_id, LifecyclePhase::Submit, &err)
                            .await;
                        return Err(err);
                    }

                    tool_cost = add_cost(tool_cost, &output.cost);
                    tool_receipts.push(ToolCallReceipt {
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        arguments_digest: Sha256Digest::of(
                            &serde_json::to_vec(&call.arguments).unwrap_or_default(),
                        ),
                        output_digest: Sha256Digest::of(
                            &serde_json::to_vec(&output.content).unwrap_or_default(),
                        ),
                        cost: runtime_cost_to_receipt(&output.cost),
                    });
                    tool_messages.push(ChatMessage::tool_result(
                        &call.id,
                        tool_output_text(&output.content),
                    ));
                }
            } else if exhausted {
                // Record the model's requested calls for audit even though the
                // loop aborts without executing them (the provider call that asked
                // for them did happen and is being receipted + billed below).
                for call in &requested {
                    tool_receipts.push(ToolCallReceipt {
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        arguments_digest: Sha256Digest::of(
                            &serde_json::to_vec(&call.arguments).unwrap_or_default(),
                        ),
                        output_digest: Sha256Digest::of(b""),
                        cost: runtime_cost_to_receipt(&RuntimeCostTuple::default()),
                    });
                }
            }

            // 6. receipt: mint over the provider response, recording the tool
            //    calls and the combined (provider + tool) cost, chained onto the
            //    prior receipt.
            let combined_cost = add_cost(response.cost, &tool_cost);
            let parent_hash = *self.chain_tail.lock();
            let body = ReceiptBody {
                receipt_id: uuid::Uuid::new_v4(),
                parent_hash,
                verb: self.verb.clone(),
                issued_at: ardur_receipt::UnixTsMillis(now_ms),
                subject: ardur_receipt::HolderId(claims.subject.0.clone()),
                cap_token_id: ardur_receipt::TokenId(claims.token_id.to_string()),
                payload_digest: Sha256Digest::of(response.content.as_bytes()),
                cost: runtime_cost_to_receipt(&combined_cost),
                tool_calls: tool_receipts,
                provider: Some(self.provider.name()),
            };
            let signed = match ReceiptSigner::sign(body, &self.receipt_key) {
                Ok(signed) => signed,
                Err(e) => {
                    self.release(reservation).await;
                    self.fire_error(session_id, LifecyclePhase::Receipt, &e)
                        .await;
                    return Err(RuntimeError::Internal(anyhow::anyhow!(
                        "receipt mint failed: {e}"
                    )));
                }
            };
            let receipt = match self
                .commit_receipt_and_journal(session_id, iteration, &req, &response, &signed, now_ms)
                .await
            {
                Ok(receipt) => receipt,
                Err(err) => {
                    self.release(reservation).await;
                    self.fire_error(session_id, LifecyclePhase::Receipt, &err)
                        .await;
                    return Err(err);
                }
            };

            // 7. post-receipt hooks (observational; the call already happened).
            let post_ctx = PostReceiptCtx {
                session_id,
                signed_receipt: &signed,
                receipt: &receipt,
                response: &response,
                cost: combined_cost,
            };
            for err in self.registry.run_post_receipt(&post_ctx).await {
                tracing::warn!(error = %err, "post-receipt hook error (non-fatal)");
            }

            // 8. cost-gate finalize: settle this iteration against the combined
            //    actual (provider + the tools it triggered).
            let actual = runtime_cost_to_gate(&combined_cost);
            match self.gate.finalize(reservation, actual).await {
                Ok(_) => {}
                Err(e) => {
                    let err = map_admission_error(e);
                    self.fire_error(session_id, LifecyclePhase::CostGate, &err)
                        .await;
                    return Err(err);
                }
            }

            // 9. memory: record this round as a bi-temporal fact. Non-fatal, but
            //    we RE-VERIFY the cap token specifically for `memory.write` to
            //    prevent attenuation bypass — the turn-level `claims` only proved
            //    `chat.submit`; a holder who attenuated away `memory.write` must
            //    not get a memory side-effect.
            if let Some(memory) = &self.memory {
                let audience = provisioning
                    .audience
                    .clone()
                    .unwrap_or_else(|| self.audience.clone());
                let memory_write_claims = CapToken::from_base64(&req.cap_token.0, &self.cap_root)
                    .and_then(|token| {
                        self.verifier.verify(
                            &token,
                            &self.cap_root,
                            &RequiredCaveats {
                                now_unix,
                                audience,
                                tool: ardur_memory::MEMORY_WRITE_CAPABILITY.to_string(),
                                cost: self.cost_units,
                            },
                        )
                    });
                match memory_write_claims {
                    Ok(mem_claims) => {
                        let record =
                            turn_record(&mem_claims.subject.0, &response, &receipt, now_ms);
                        let plane = MemoryControlPlane::new(memory.as_ref(), self.policies.clone());
                        if let Err(mem_err) = plane.record(&mem_claims, record) {
                            self.fire_error(session_id, LifecyclePhase::MemoryWrite, &mem_err)
                                .await;
                        }
                    }
                    Err(CapTokenError::ToolNotAllowed) => {
                        // Token does not grant memory.write (attenuated or never
                        // issued). Skip the memory side-effect silently — this is
                        // the intended behaviour for write-less tokens.
                    }
                    Err(CapTokenError::Expired) => {
                        let err = RuntimeError::CapTokenExpired;
                        tracing::warn!(
                            session_id = ?session_id,
                            error = %err,
                            "memory.write cap-token re-verification failed"
                        );
                        self.fire_error(session_id, LifecyclePhase::MemoryWrite, &err)
                            .await;
                    }
                    Err(other) => {
                        let err = RuntimeError::CapDenied {
                            reason: other.to_string(),
                        };
                        tracing::warn!(
                            session_id = ?session_id,
                            error = %err,
                            "memory.write cap-token re-verification failed"
                        );
                        self.fire_error(session_id, LifecyclePhase::MemoryWrite, &err)
                            .await;
                    }
                }
            }

            // 10. session-journal + receipt were committed atomically before
            //     post-receipt hooks/finalize/memory, so no separate journal append
            //     runs here.

            total_cost = add_cost(total_cost, &combined_cost);

            // Termination: a response with no tool calls is the final answer; a
            // tool-wanting response at the iteration ceiling aborts; otherwise we
            // fold the assistant's tool-call turn and the tool results into the
            // transcript and loop.
            if !wants_tools {
                break (receipt, response.content);
            }
            if exhausted {
                let err = RuntimeError::ToolLoopExhausted {
                    iterations: iteration,
                };
                self.fire_error(session_id, LifecyclePhase::Provider, &err)
                    .await;
                return Err(err);
            }
            messages.push(ChatMessage::assistant_tool_calls(
                response.content.clone(),
                requested,
            ));
            messages.extend(tool_messages);
        };

        Ok(SubmitResult {
            receipt_id: ReceiptId(receipt.receipt_id),
            response: ChatMessage::assistant(final_content),
            cost: total_cost,
        })
    }
}

impl FusedRuntime {
    /// **§6.0c.** Stream a turn through the full ten-stage pipeline, emitting a
    /// [`FusedEvent`] feed as it unfolds.
    ///
    /// This is the progressive sibling of [`submit`](ChatRuntime::submit): it
    /// runs the **same** stages over the **same** helpers (cap-token → Cedar →
    /// injection-defense → cost-gate → provider → tool-exec → receipt → finalize
    /// → memory → journal) but, instead of returning one
    /// [`SubmitResult`](SubmitResult) at the end, yields stage transitions, token
    /// [`Content`](FusedEvent::Content) deltas as the provider produces them, the
    /// tool-call lifecycle, the minted receipt's chain hash, and a terminal
    /// [`Finish`](FusedEvent::Finish). The substrate the §2.1b CLI streaming path
    /// bypassed (PR #89) is fully intact — every streamed turn is cap-verified,
    /// authorized, admitted, receipted, and journaled.
    ///
    /// The item type is `Result<FusedEvent, RuntimeError>`: a stage that rejects
    /// the turn emits its [`StageEnd { ok: false }`](FusedEvent::StageEnd) and
    /// then a terminal `Err`, after which the stream ends (the
    /// [`ProviderStream`](ardur_provider_runtime::ProviderStream) convention).
    ///
    /// **Cancellation.** The whole pipeline runs inside the returned stream's
    /// generator, so dropping the stream cancels the in-flight provider round at
    /// its next `.await`. Because the receipt is minted only *after* a round
    /// completes, a turn cancelled mid-generation mints **no** receipt, appends
    /// **no** journal entry, and records **no** memory — the held reservation
    /// lapses on the gate's TTL. See [`crate::streaming`] for the full contract.
    pub fn stream(
        &self,
        req: SubmitRequest,
    ) -> impl Stream<Item = Result<FusedEvent, RuntimeError>> + Send + '_ {
        self.stream_inner(req, PerRequestProvisioning::default())
    }

    /// Streaming counterpart of
    /// [`submit_with_provisioning`](Self::submit_with_provisioning): drive the
    /// [`stream`](Self::stream) pipeline with per-request budget / audience /
    /// subject overrides. `stream(req)` is exactly
    /// `stream_with_provisioning(req, Default::default())`.
    pub fn stream_with_provisioning(
        &self,
        req: SubmitRequest,
        provisioning: PerRequestProvisioning,
    ) -> impl Stream<Item = Result<FusedEvent, RuntimeError>> + Send + '_ {
        self.stream_inner(req, provisioning)
    }

    /// The shared streaming pipeline. Mirrors [`submit_inner`](Self::submit_inner)
    /// stage-for-stage, reusing the same stage helpers, scan methods, cost/error
    /// conversions, and receipt/journal logic — the only difference is that it
    /// emits events progressively instead of accumulating a single result.
    fn stream_inner(
        &self,
        req: SubmitRequest,
        provisioning: PerRequestProvisioning,
    ) -> impl Stream<Item = Result<FusedEvent, RuntimeError>> + Send + '_ {
        async_stream::try_stream! {
            let session_id = req.session_id;
            let now_ms = self.clock.now_ms();
            let now_unix = now_ms / 1000;

            // ---- 1. cap-token.
            yield FusedEvent::StageStart { stage: StageKind::CapTokenVerify };
            let claims = match self.stage_cap_token(&req, &provisioning, now_unix) {
                Ok(claims) => {
                    yield FusedEvent::StageEnd { stage: StageKind::CapTokenVerify, ok: true };
                    claims
                }
                Err(err) => {
                    yield FusedEvent::StageEnd { stage: StageKind::CapTokenVerify, ok: false };
                    self.fire_error(session_id, LifecyclePhase::Submit, &err).await;
                    Err(err)?
                }
            };

            // ---- 2. cedar-policy.
            yield FusedEvent::StageStart { stage: StageKind::CedarCheck };
            if let Err(err) = self.stage_cedar(session_id, &claims) {
                yield FusedEvent::StageEnd { stage: StageKind::CedarCheck, ok: false };
                self.fire_error(session_id, LifecyclePhase::Submit, &err).await;
                Err(err)?;
            }
            yield FusedEvent::StageEnd { stage: StageKind::CedarCheck, ok: true };

            // ---- 3. cost-gate setup (provision + bind; no per-round admission
            //         yet — that happens inside the loop). No stage event: the
            //         CostGateAdmit event brackets the per-round `admit`.
            let gate_token_id = match self.stage_cost_setup(&claims, &provisioning).await {
                Ok(gate_token_id) => gate_token_id,
                Err(err) => {
                    self.fire_error(session_id, LifecyclePhase::Submit, &err).await;
                    Err(err)?
                }
            };

            let tool_defs = self.tool_defs();

            // ---- 4. pre-submit hooks. A veto needs no release (no reservation
            //         is held) and fires no error hook (matching `submit`).
            let initial = match self.stage_pre_submit(&req, tool_defs.clone()).await {
                Ok(request) => request,
                Err(err) => {
                    Err(err)?;
                    unreachable!()
                }
            };
            let initial = match self.inject_recalled_memories(initial, &claims) {
                Ok(request) => request,
                Err(err) => {
                    self.fire_error(session_id, LifecyclePhase::MemoryWrite, &err).await;
                    Err(err)?;
                    unreachable!()
                }
            };

            let mut messages = initial.messages;
            let temperature = initial.temperature;
            let stop_sequences = initial.stop_sequences;
            let requested_cost_envelope = initial.requested_cost_envelope;
            let mut iteration: u32 = 0;

            // The terminal finish reason of the round that settles the turn.
            let final_finish = loop {
                iteration += 1;

                let mut iter_request =
                    CompletionRequest::new(messages.clone(), self.model.clone(), self.max_tokens);
                iter_request.temperature = temperature;
                iter_request.stop_sequences = stop_sequences.clone();
                iter_request.requested_cost_envelope = requested_cost_envelope;
                iter_request.tools = tool_defs.clone();
                iter_request.stream = true;

                // 4.5 injection-defense scan (most recent user message).
                yield FusedEvent::StageStart { stage: StageKind::InjectionScan };
                let iter_request = match self.scan_outbound_request(iter_request).await {
                    Ok(request) => {
                        yield FusedEvent::StageEnd { stage: StageKind::InjectionScan, ok: true };
                        request
                    }
                    Err(err) => {
                        yield FusedEvent::StageEnd { stage: StageKind::InjectionScan, ok: false };
                        self.fire_error(session_id, LifecyclePhase::Submit, &err).await;
                        Err(err)?
                    }
                };

                // 3'. cost-gate admit (per iteration).
                yield FusedEvent::StageStart { stage: StageKind::CostGateAdmit };
                let request_digest =
                    GateSha256::of(&serde_json::to_vec(&iter_request.messages).unwrap_or_default());
                let reservation_handle = match self
                    .gate
                    .admit(AdmissionRequest {
                        cap_token_id: gate_token_id,
                        projected_envelope: self.envelope,
                        provider_id: self.gate_provider_id.clone(),
                        model_id: self.gate_model_id.clone(),
                        request_digest,
                    })
                    .await
                {
                    Ok(reservation) => {
                        yield FusedEvent::StageEnd { stage: StageKind::CostGateAdmit, ok: true };
                        reservation
                    }
                    Err(e) => {
                        let err = map_admission_error(e);
                        yield FusedEvent::StageEnd { stage: StageKind::CostGateAdmit, ok: false };
                        self.fire_error(session_id, LifecyclePhase::Submit, &err).await;
                        Err(err)?
                    }
                };
                // Hold the reservation in an `Option` so the borrow checker is
                // happy with it being consumed on whichever (mutually exclusive)
                // path settles the round — `try_stream!`'s `?` desugaring hides
                // the divergence of the error paths from NLL, so a bare move
                // would look like a double-move across loop iterations. `.take()`
                // moves the value out without moving the binding.
                // ARD-488: release the reservation if this streaming round is
                // cancelled (stream dropped / outer timeout) before it settles.
                let _cancel_guard = ReservationCancelGuard::new(
                    Arc::clone(&self.gate),
                    self.budget.clone(),
                    reservation_handle.reservation_id,
                );
                let mut reservation = Some(reservation_handle);

                // 5. provider stream: forward each delta as it arrives.
                yield FusedEvent::StageStart { stage: StageKind::ProviderStream };
                let mut provider_stream = match self.provider.stream(iter_request).await {
                    Ok(provider_stream) => provider_stream,
                    Err(provider_err) => {
                        self.release(reservation.take().expect("reservation held")).await;
                        yield FusedEvent::StageEnd { stage: StageKind::ProviderStream, ok: false };
                        self.fire_error(session_id, LifecyclePhase::Provider, &provider_err)
                            .await;
                        Err(map_provider_error(&provider_err))?
                    }
                };
                let mut content = String::new();
                let mut usage = Usage::default();
                let mut finish_reason = FinishReason::Stop;
                let mut stream_err: Option<ProviderError> = None;
                while let Some(item) = provider_stream.next().await {
                    match item {
                        Ok(StreamEvent::ContentDelta(text)) => {
                            content.push_str(&text);
                            yield FusedEvent::Content(text);
                        }
                        Ok(StreamEvent::ToolCallStart(call)) => {
                            yield FusedEvent::ToolCallStart {
                                id: call.id,
                                name: call.name,
                            };
                        }
                        Ok(StreamEvent::ToolCallDelta { id, delta }) => {
                            yield FusedEvent::ToolCallDelta { id, delta };
                        }
                        Ok(StreamEvent::Usage(reported)) => usage = reported,
                        Ok(StreamEvent::Finish(reason)) => finish_reason = reason,
                        Err(provider_err) => {
                            stream_err = Some(provider_err);
                            break;
                        }
                    }
                }
                // Free the provider stream before the post-provider stages run.
                drop(provider_stream);
                if let Some(provider_err) = stream_err {
                    self.release(reservation.take().expect("reservation held")).await;
                    yield FusedEvent::StageEnd { stage: StageKind::ProviderStream, ok: false };
                    self.fire_error(session_id, LifecyclePhase::Provider, &provider_err)
                        .await;
                    Err(map_provider_error(&provider_err))?;
                }
                yield FusedEvent::Usage(usage);
                yield FusedEvent::StageEnd { stage: StageKind::ProviderStream, ok: true };

                // Assemble the round's response, priced from the rate card exactly
                // as a non-streaming `complete()` would have returned it.
                let response = CompletionResponse {
                    content: content.clone(),
                    finish_reason: finish_reason.clone(),
                    usage,
                    cost: self.provider.rate_card().price(usage),
                    raw_provider_response: None,
                };

                let requested: Vec<ToolCall> = match &response.finish_reason {
                    FinishReason::ToolUse(calls) => calls.clone(),
                    _ => Vec::new(),
                };
                let wants_tools = !requested.is_empty();
                let exhausted = wants_tools && iteration >= self.max_tool_iterations;

                // 6. tool execution.
                let mut tool_receipts: Vec<ToolCallReceipt> = Vec::new();
                let mut tool_messages: Vec<ChatMessage> = Vec::new();
                let mut tool_cost = RuntimeCostTuple::default();
                if wants_tools && !exhausted {
                    yield FusedEvent::StageStart { stage: StageKind::ToolExec };
                    for call in &requested {
                        let Some(tool) = self.tools.get(&ToolId::new(&call.name)) else {
                            self.release(reservation.take().expect("reservation held")).await;
                            let err = RuntimeError::UnknownTool {
                                tool: call.name.clone(),
                            };
                            yield FusedEvent::StageEnd { stage: StageKind::ToolExec, ok: false };
                            self.fire_error(session_id, LifecyclePhase::Provider, &err).await;
                            Err(err)?;
                            unreachable!()
                        };
                        if let Err(err) = self.authorize_tool_invocation(
                            &req,
                            &provisioning,
                            session_id,
                            now_unix,
                            &call.name,
                        ) {
                            self.release(reservation.take().expect("reservation held")).await;
                            yield FusedEvent::StageEnd { stage: StageKind::ToolExec, ok: false };
                            self.fire_error(session_id, LifecyclePhase::Submit, &err).await;
                            Err(err)?;
                            unreachable!()
                        }
                        // ARD-420: enforce required_capabilities() against the
                        // cap-token's tool allowlist before the tool body runs.
                        if let Err(err) = self.authorize_tool_capabilities(
                            &req,
                            &provisioning,
                            now_unix,
                            &call.name,
                            tool.required_capabilities(),
                        ) {
                            self.release(reservation.take().expect("reservation held")).await;
                            yield FusedEvent::StageEnd { stage: StageKind::ToolExec, ok: false };
                            self.fire_error(session_id, LifecyclePhase::Submit, &err).await;
                            Err(err)?;
                            unreachable!()
                        }
                        let ctx = self.tool_context(&req.cap_token, session_id);
                        let output = match tokio::time::timeout(
                            self.tool_timeout,
                            tool.invoke(&ctx, call.arguments.clone()),
                        )
                        .await
                        {
                            Ok(Ok(output)) => output,
                            Ok(Err(tool_err)) => {
                                self.release(reservation.take().expect("reservation held")).await;
                                let err = map_tool_error(tool_err, &call.name);
                                yield FusedEvent::StageEnd { stage: StageKind::ToolExec, ok: false };
                                self.fire_error(session_id, LifecyclePhase::Provider, &err).await;
                                Err(err)?;
                                unreachable!()
                            }
                            Err(_elapsed) => {
                                self.release(reservation.take().expect("reservation held")).await;
                                let err = RuntimeError::ToolTimeout {
                                    tool: call.name.clone(),
                                };
                                yield FusedEvent::StageEnd { stage: StageKind::ToolExec, ok: false };
                                self.fire_error(session_id, LifecyclePhase::Provider, &err).await;
                                Err(err)?;
                                unreachable!()
                            }
                        };

                        // Scan the tool output before it re-enters the context.
                        if let Err(err) = self.scan_tool_output(&call.name, &output.content).await {
                            self.release(reservation.take().expect("reservation held")).await;
                            yield FusedEvent::StageEnd { stage: StageKind::ToolExec, ok: false };
                            self.fire_error(session_id, LifecyclePhase::Submit, &err).await;
                            Err(err)?;
                        }

                        yield FusedEvent::ToolCallResult {
                            id: call.id.clone(),
                            result: output.content.clone(),
                        };

                        tool_cost = add_cost(tool_cost, &output.cost);
                        tool_receipts.push(ToolCallReceipt {
                            call_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            arguments_digest: Sha256Digest::of(
                                &serde_json::to_vec(&call.arguments).unwrap_or_default(),
                            ),
                            output_digest: Sha256Digest::of(
                                &serde_json::to_vec(&output.content).unwrap_or_default(),
                            ),
                            cost: runtime_cost_to_receipt(&output.cost),
                        });
                        tool_messages.push(ChatMessage::tool_result(
                            &call.id,
                            tool_output_text(&output.content),
                        ));
                    }
                    yield FusedEvent::StageEnd { stage: StageKind::ToolExec, ok: true };
                } else if exhausted {
                    // Record the requested calls for audit even though the loop
                    // aborts without executing them (mirrors `submit`).
                    for call in &requested {
                        tool_receipts.push(ToolCallReceipt {
                            call_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            arguments_digest: Sha256Digest::of(
                                &serde_json::to_vec(&call.arguments).unwrap_or_default(),
                            ),
                            output_digest: Sha256Digest::of(b""),
                            cost: runtime_cost_to_receipt(&RuntimeCostTuple::default()),
                        });
                    }
                }

                // 7. receipt mint + chain.
                yield FusedEvent::StageStart { stage: StageKind::ReceiptMint };
                let combined_cost = add_cost(response.cost, &tool_cost);
                let parent_hash = *self.chain_tail.lock();
                let body = ReceiptBody {
                    receipt_id: uuid::Uuid::new_v4(),
                    parent_hash,
                    verb: self.verb.clone(),
                    issued_at: ardur_receipt::UnixTsMillis(now_ms),
                    subject: ardur_receipt::HolderId(claims.subject.0.clone()),
                    cap_token_id: ardur_receipt::TokenId(claims.token_id.to_string()),
                    payload_digest: Sha256Digest::of(response.content.as_bytes()),
                    cost: runtime_cost_to_receipt(&combined_cost),
                    tool_calls: tool_receipts,
                    provider: Some(self.provider.name()),
                };
                let signed = match ReceiptSigner::sign(body, &self.receipt_key) {
                    Ok(signed) => signed,
                    Err(e) => {
                        self.release(reservation.take().expect("reservation held")).await;
                        yield FusedEvent::StageEnd { stage: StageKind::ReceiptMint, ok: false };
                        self.fire_error(session_id, LifecyclePhase::Receipt, &e).await;
                        Err(RuntimeError::Internal(anyhow::anyhow!(
                            "receipt mint failed: {e}"
                        )))?;
                        unreachable!()
                    }
                };
                let receipt = match self
                    .commit_receipt_and_journal(session_id, iteration, &req, &response, &signed, now_ms)
                    .await
                {
                    Ok(receipt) => receipt,
                    Err(err) => {
                        self.release(reservation.take().expect("reservation held")).await;
                        self.fire_error(session_id, LifecyclePhase::Receipt, &err).await;
                        yield FusedEvent::StageEnd { stage: StageKind::ReceiptMint, ok: false };
                        Err(err)?;
                        unreachable!()
                    }
                };
                let chain_hash = Sha256Digest::of(signed.jws_compact().as_bytes());
                yield FusedEvent::Receipt {
                    receipt_id: ReceiptId(receipt.receipt_id),
                    chain_hash: format!("{chain_hash}"),
                };
                yield FusedEvent::StageEnd { stage: StageKind::ReceiptMint, ok: true };

                // 7'. post-receipt hooks (observational).
                let post_ctx = PostReceiptCtx {
                    session_id,
                    signed_receipt: &signed,
                    receipt: &receipt,
                    response: &response,
                    cost: combined_cost,
                };
                for err in self.registry.run_post_receipt(&post_ctx).await {
                    tracing::warn!(error = %err, "post-receipt hook error (non-fatal)");
                }

                // 8. cost-gate finalize.
                yield FusedEvent::StageStart { stage: StageKind::CostGateFinalize };
                let actual = runtime_cost_to_gate(&combined_cost);
                match self
                    .gate
                    .finalize(reservation.take().expect("reservation held"), actual)
                    .await
                {
                    Ok(_) => {
                        yield FusedEvent::StageEnd { stage: StageKind::CostGateFinalize, ok: true };
                    }
                    Err(e) => {
                        let err = map_admission_error(e);
                        self.fire_error(session_id, LifecyclePhase::CostGate, &err).await;
                        yield FusedEvent::StageEnd { stage: StageKind::CostGateFinalize, ok: false };
                        Err(err)?;
                        unreachable!()
                    }
                }

                // 9. memory (only when a backend is configured). We RE-VERIFY the
                // cap token specifically for `memory.write` to prevent attenuation
                // bypass — the turn-level claims only proved `chat.submit`.
                // The write remains non-fatal; a token that lacks `memory.write`
                // (attenuated or never issued) silently skips the side-effect.
                if let Some(memory) = &self.memory {
                    yield FusedEvent::StageStart { stage: StageKind::MemoryRecord };
                    let audience = provisioning
                        .audience
                        .clone()
                        .unwrap_or_else(|| self.audience.clone());
                    let memory_write_claims = CapToken::from_base64(&req.cap_token.0, &self.cap_root)
                        .and_then(|token| {
                            self.verifier.verify(
                                &token,
                                &self.cap_root,
                                &RequiredCaveats {
                                    now_unix,
                                    audience,
                                    tool: ardur_memory::MEMORY_WRITE_CAPABILITY.to_string(),
                                    cost: self.cost_units,
                                },
                            )
                        });
                    match memory_write_claims {
                        Ok(mem_claims) => {
                            let record = turn_record(&mem_claims.subject.0, &response, &receipt, now_ms);
                            let plane = MemoryControlPlane::new(memory.as_ref(), self.policies.clone());
                            if let Err(mem_err) = plane.record(&mem_claims, record) {
                                self.fire_error(session_id, LifecyclePhase::MemoryWrite, &mem_err)
                                    .await;
                            }
                        }
                        Err(CapTokenError::ToolNotAllowed) => {
                            // Token does not grant memory.write — skip silently.
                        }
                        Err(CapTokenError::Expired) => {
                            let err = RuntimeError::CapTokenExpired;
                            tracing::warn!(
                                session_id = ?session_id,
                                error = %err,
                                "memory.write cap-token re-verification failed"
                            );
                            self.fire_error(session_id, LifecyclePhase::MemoryWrite, &err)
                                .await;
                        }
                        Err(other) => {
                            let err = RuntimeError::CapDenied {
                                reason: other.to_string(),
                            };
                            tracing::warn!(
                                session_id = ?session_id,
                                error = %err,
                                "memory.write cap-token re-verification failed"
                            );
                            self.fire_error(session_id, LifecyclePhase::MemoryWrite, &err)
                                .await;
                        }
                    }
                    yield FusedEvent::StageEnd { stage: StageKind::MemoryRecord, ok: true };
                }

                // 10. session-journal + receipt were committed atomically before
                //     post-receipt hooks/finalize/memory, so no separate journal
                //     append runs here.

                // Termination: a response with no tool calls settles the turn; a
                // tool-wanting response at the ceiling aborts; otherwise fold the
                // tool round into the transcript and loop.
                if !wants_tools {
                    break finish_reason;
                }
                if exhausted {
                    let err = RuntimeError::ToolLoopExhausted {
                        iterations: iteration,
                    };
                    self.fire_error(session_id, LifecyclePhase::Provider, &err).await;
                    Err(err)?;
                    unreachable!()
                }
                messages.push(ChatMessage::assistant_tool_calls(
                    response.content.clone(),
                    requested,
                ));
                messages.extend(tool_messages);
            };

            yield FusedEvent::Finish(final_finish);
        }
    }
}

/// Map a cost-gate admission failure onto the runtime's error surface. Every
/// budget/ceiling rejection is a cost-ceiling outcome; a malformed binding or an
/// internal fault degrade to their nearest runtime variant.
fn map_admission_error(err: AdmissionError) -> RuntimeError {
    match err {
        AdmissionError::BudgetExhausted { .. }
        | AdmissionError::PolicyDenied(_)
        | AdmissionError::ReservationExpired => RuntimeError::CostCeilingExceeded,
        AdmissionError::CapTokenInvalid => RuntimeError::CapDenied {
            reason: "cost-gate could not resolve the cap-token to a holder".to_string(),
        },
        AdmissionError::ProviderNotAllowed(provider) => {
            RuntimeError::Internal(anyhow::anyhow!("provider not allowed: {provider:?}"))
        }
        AdmissionError::Internal(inner) => RuntimeError::Internal(inner),
    }
}

/// Derive the Cedar principal from a verified cap-token's subject. The entity
/// *type* (`User`, `Agent`, …) is the one structural knob — it is part of the
/// runtime's identity model, not the request — while the entity *id* is the
/// verified subject, quoted so an id carrying `:` or `/` (a SPIFFE URI) is a
/// single Cedar string literal. The caller never supplies the principal, so it
/// cannot assert an identity the cap-token did not prove.
fn derive_principal(entity_type: &str, claims: &VerifiedClaims) -> PrincipalRef {
    PrincipalRef(format!("{entity_type}::\"{}\"", claims.subject.0))
}

/// Derive the Cedar resource from the request's session: a turn acts upon the
/// session it belongs to, and the session id is verified request metadata
/// already threaded through every stage — so `Session::"<uuid>"` is a concrete,
/// per-request resource rather than a static placeholder.
fn derive_resource(session_id: SessionId) -> ResourceRef {
    ResourceRef(format!("Session::\"{}\"", session_id.0))
}

/// Project a verified cap-token's claims onto the Cedar resource attributes so
/// policies can gate on the proven facts (`resource.audience`,
/// `resource.tools`, `resource.expires_unix`, `resource.subject`,
/// `resource.budget_remaining`). The cedar-policy crate channels evaluation
/// attributes through the resource entity (its `Context` is always empty), so
/// the cap "context" surfaces as `resource.<key>`, not `context.<key>`.
///
/// Builder-supplied [`cedar_attributes`](crate::FusedRuntimeBuilder::cedar_attributes)
/// form the base, but the verified claim keys are layered on top and win on any
/// collision — a caller cannot shadow a proven fact (e.g. spoof
/// `resource.audience`) through static attributes.
fn cedar_attributes_from_claims(
    base: &serde_json::Value,
    claims: &VerifiedClaims,
) -> serde_json::Value {
    let mut map = match base {
        serde_json::Value::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    map.insert("subject".to_string(), claims.subject.0.clone().into());
    map.insert("audience".to_string(), claims.audience.clone().into());
    map.insert(
        "tools".to_string(),
        serde_json::Value::Array(
            claims
                .tool_allowlist
                .iter()
                .map(|t| serde_json::Value::String(t.clone()))
                .collect(),
        ),
    );
    map.insert("expires_unix".to_string(), claims.expires_unix.into());
    map.insert(
        "budget_remaining".to_string(),
        claims.budget_remaining.into(),
    );
    serde_json::Value::Object(map)
}

/// Map a provider failure onto the runtime's error surface.
fn map_provider_error(err: &ProviderError) -> RuntimeError {
    match err {
        ProviderError::CostCeilingExceeded => RuntimeError::CostCeilingExceeded,
        _ => RuntimeError::ProviderUnavailable,
    }
}

/// Sum two runtime cost tuples dimension-wise (saturating on the integer axes),
/// used to fold each tool call's cost into a turn's provider cost (§6.0).
fn add_cost(a: RuntimeCostTuple, b: &RuntimeCostTuple) -> RuntimeCostTuple {
    RuntimeCostTuple {
        tokens_in: a.tokens_in.saturating_add(b.tokens_in),
        tokens_out: a.tokens_out.saturating_add(b.tokens_out),
        cents: a.cents.saturating_add(b.cents),
        wall_ms: a.wall_ms.saturating_add(b.wall_ms),
        attention_score: a.attention_score + b.attention_score,
    }
}

/// Map a tool-registry failure onto the runtime's error surface. A tool that
/// reported its own timeout maps to [`RuntimeError::ToolTimeout`]; everything
/// else degrades to [`RuntimeError::Internal`] carrying the tool name.
fn map_tool_error(err: ToolError, tool: &str) -> RuntimeError {
    match err {
        ToolError::Timeout => RuntimeError::ToolTimeout {
            tool: tool.to_string(),
        },
        other => RuntimeError::Internal(anyhow::anyhow!("tool `{tool}` failed: {other}")),
    }
}

/// Render a tool's JSON output as the text content of its `tool_result` message.
/// A JSON string is unwrapped to its inner text; anything else is its compact
/// JSON rendering.
fn tool_output_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Convert the runtime's `CostTuple` into the receipt crate's (identically
/// shaped) `CostTuple`.
fn runtime_cost_to_receipt(cost: &RuntimeCostTuple) -> ardur_receipt::CostTuple {
    ardur_receipt::CostTuple {
        tokens_in: cost.tokens_in,
        tokens_out: cost.tokens_out,
        cents: cost.cents,
        wall_ms: cost.wall_ms,
        attention_score: cost.attention_score,
    }
}

/// Widen the runtime's `CostTuple` into the cost gate's, mapping the fractional
/// attention score onto the gate's integer axis.
fn runtime_cost_to_gate(cost: &RuntimeCostTuple) -> GateCostTuple {
    GateCostTuple {
        tokens_in: cost.tokens_in,
        tokens_out: cost.tokens_out,
        cents: cost.cents,
        wall_ms: cost.wall_ms,
        attention_score: cost.attention_score as u64,
    }
}

/// Build the bi-temporal memory record for a completed turn.
fn turn_record(
    subject: &str,
    response: &CompletionResponse,
    receipt: &ReceiptBody,
    now_ms: u64,
) -> ardur_memory::MemoryRecord {
    let now = ardur_memory::UnixTsMillis(now_ms);
    let mut record = ardur_memory::MemoryRecord::new(
        ardur_memory::HolderId(subject.to_string()),
        ardur_memory::RecordKind::Observation,
        serde_json::json!({
            "response": response.content,
            "receipt_id": receipt.receipt_id,
            "source": "turn",
            "workspace_id": subject,
            "confidence": 1.0,
        }),
        now,
        now,
        None,
        now,
    );
    record.source_receipt_id = Some(ardur_memory::ReceiptId(receipt.receipt_id));
    record
}

/// Render a memory payload into a compact context string.
fn memory_payload_text(payload: &serde_json::Value) -> String {
    if let Some(object) = payload.get("object") {
        return match object {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    match payload {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The most recent user message in a transcript — the prompt journaled for the
/// turn.
fn last_user_message(messages: &[ChatMessage]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::User))
        .map(|m| m.content.as_str())
}
