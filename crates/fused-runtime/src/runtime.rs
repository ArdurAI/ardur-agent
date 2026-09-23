//! [`FusedRuntime`] — the fused [`ChatRuntime`] and its multi-stage [`submit`]
//! pipeline (cap-token → cedar → cost-gate → pre-submit hooks → injection-defense
//! → provider → receipt → post-receipt hooks → finalize → memory → journal). See
//! the crate root for the full stage list and the Option-B rationale.
//!
//! [`submit`]: FusedRuntime::submit

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use ardur_approvals::{
    ApprovalStatus, ApprovalStore, ClaimBinding, ClaimOutcome, InvocationOutcome, InvocationResult,
};
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
    ModelId as GateModelId, ProviderId as GateProviderId, Sha256Digest as GateSha256,
    TokenId as GateTokenId,
};
use ardur_governance::{
    ActionClass as ErActionClass, AuthOutcome as ErAuthOutcome, CompletedOutcome, DeniedOutcome,
    ErRoundFacts, EventOutcome, EvidenceOutputAdmission, GovernanceEmitter, GrantFacts,
    InvocationClassification, MirroredToolCall, PostEffectRecord, PreEffectRecord,
    PublicDenialReason as ErPublicDenialReason, SideEffectClass as ErSideEffectClass,
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
    Es256SigningKey, ReceiptBody, ReceiptSigner, Sha256Digest, SignedReceipt, ToolCallReceipt,
    VerbObject,
};
use ardur_runtime::{
    CapTokenRef, ChatMessage, ChatRuntime, CostTuple as RuntimeCostTuple, ReceiptId, Role,
    RuntimeError, SessionId, SubmitRequest, SubmitResult, ToolCall,
};
use ardur_session_journals::{JournalEntry, SessionJournal};
use ardur_tool_registry::{Capability, InvocationId, ToolContext, ToolError, ToolId, ToolRegistry};
use parking_lot::Mutex;

use crate::receipts::{
    PersistedReceipt, load_persisted_chain, replace_receipt_log_no_follow,
    verify_persisted_chain_with_jwks,
};
use crate::reconcile::{
    ReconciliationAction, ReconciliationError, ReconciliationReport, ReconciliationStrategy,
};
use crate::settlement::{
    Disposition, SettlementCoordinator, SettlementError, SettlementSupervisor, TurnIdentity,
    TurnSettlementOwner,
};
use crate::shared::{SharedBudget, SharedDenyList};
use crate::streaming::{FusedEvent, StageKind};
use ardur_session_journals::settlement::{
    CostProvenance, InfrastructureFailureClass, OutputAdmission, ProviderEvidence, ReceiptBinding,
    ReceiptCandidate, RefusalClass, ToolEffect, ToolEvidence, ToolFailureClass, UsageSnapshot,
};
use futures::{Stream, StreamExt as _};
use tokio::sync::Mutex as AsyncMutex;

/// The receipt verb minted for a completed turn (`verb.object.state.vN`).
pub(crate) const COMPLETION_VERB: &str = "llm.completion.minted.v1";

/// The terminal verb recorded when a turn is abandoned after at least one
/// tool-loop round already committed a receipt (#422). The receipt log is
/// append-only, so earlier rounds cannot be un-minted; this marks the chain so
/// an intermediate round is never the last word on a turn that never settled.
pub(crate) const CANCELLED_VERB: &str = "llm.completion.cancelled.v1";

/// gh#533: the receipt verb an applied compaction settles under. The receipt
/// is the settlement receipt of the paid summarization call (carrying the
/// observed provider cost), not a zero-cost marker.
pub(crate) const COMPACT_APPLIED_VERB: &str = "context.compact.applied.v1";
/// gh#533: the receipt verb a compaction preview settles under. A preview is
/// non-mutating to conversation history but IS an external provider send, so
/// it must leave the same durable audit trail an applied compaction does.
pub(crate) const COMPACT_PREVIEWED_VERB: &str = "context.compact.previewed.v1";
/// gh#533: the receipt verb a completed background task settles under.
pub(crate) const TASK_COMPLETED_VERB: &str = "task.background.completed.v1";
/// gh#533: the receipt verb a failed background task settles under.
pub(crate) const TASK_FAILED_VERB: &str = "task.background.failed.v1";

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

/// **§1.8.** The result of [`FusedRuntime::checkpoint`].
#[derive(Clone, Debug)]
pub struct CheckpointOutcome {
    /// The newly minted checkpoint's stable id.
    pub checkpoint_id: uuid::Uuid,
    /// The journal position the checkpoint entry landed at.
    pub entry_id: ardur_session_journals::EntryId,
    /// The receipt chained for this checkpoint.
    pub receipt_id: ReceiptId,
    /// The (possibly caller-supplied, otherwise generated) checkpoint label.
    pub summary: String,
}

/// **§1.8.** One checkpoint entry, as returned by [`FusedRuntime::list_checkpoints`].
#[derive(Clone, Debug)]
pub struct CheckpointInfo {
    /// The checkpoint's stable id.
    pub checkpoint_id: uuid::Uuid,
    /// The journal position this checkpoint was recorded at.
    pub entry_id: ardur_session_journals::EntryId,
    /// The checkpoint's label.
    pub summary: String,
    /// When the checkpoint was recorded.
    pub at: u64,
}

/// **§1.8.** The result of [`FusedRuntime::rollback_to_checkpoint`].
#[derive(Clone, Debug)]
pub struct RollbackOutcome {
    /// The checkpoint that was rolled back to.
    pub target_checkpoint_id: uuid::Uuid,
    /// The journal position the `Rollback` marker landed at.
    pub entry_id: ardur_session_journals::EntryId,
    /// The receipt chained for this rollback.
    pub receipt_id: ReceiptId,
    /// The journal entries up to and including the target checkpoint — the
    /// caller's new live-history view.
    pub retained_entries: Vec<JournalEntry>,
}

/// **§1.7.** The result of [`FusedRuntime::compact`].
#[derive(Clone, Debug)]
pub struct CompactOutcome {
    /// The newly minted compaction checkpoint's stable id (restore it with
    /// [`FusedRuntime::rollback_to_checkpoint`]).
    pub checkpoint_id: uuid::Uuid,
    /// The journal position the checkpoint entry landed at.
    pub entry_id: ardur_session_journals::EntryId,
    /// The receipt chained for this compaction.
    pub receipt_id: ReceiptId,
    /// The structured compaction summary text.
    pub summary: String,
    /// A rough (~4 chars/token) estimate of the pre-compaction history size.
    pub before_tokens_estimate: u64,
    /// A rough (~4 chars/token) estimate of the summary's size.
    pub after_tokens_estimate: u64,
}

/// **§1.9.** The result of [`FusedRuntime::run_background_task`]. Exactly one
/// of `result`/`error` is `Some` — a background task's own success/failure is
/// a normal terminal outcome, not a [`RuntimeError`] (see the method's docs).
#[derive(Clone, Debug)]
pub struct BackgroundTaskOutcome {
    /// The terminal receipt chained for this task (completed or failed).
    pub receipt_id: ReceiptId,
    /// The task's result, on success.
    pub result: Option<String>,
    /// The failure message, on failure.
    pub error: Option<String>,
}

// A round borrows the one supervised turn owner. It never contains a raw
// Reservation, so legacy release/finalize cannot race claimed authority.
struct RuntimeReservation<'a> {
    owner: &'a mut TurnSettlementOwner,
    reservation_id: uuid::Uuid,
}

fn settlement_error(error: SettlementError) -> RuntimeError {
    if let SettlementError::Budget(e) = &error {
        if matches!(
            e.as_ref(),
            AdmissionError::BudgetExhausted { .. }
                | AdmissionError::PolicyDenied(_)
                | AdmissionError::ReservationExpired
        ) {
            return RuntimeError::CostCeilingExceeded;
        }
    }
    RuntimeError::Internal(anyhow::anyhow!(error))
}

/// Runtime implementation that fuses authorization, provider execution, cost
/// admission, receipt durability, journaling, and memory behind one entry point.
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
    pub(crate) settlements: Arc<SettlementCoordinator>,
    /// Cooperatively serialize economic admission without allocating waiting
    /// settlement slots. Held through projection, never through final delivery.
    pub(crate) economic_admission: AsyncMutex<()>,
    pub(crate) budget: SharedBudget,
    pub(crate) gate_provider_id: GateProviderId,
    pub(crate) gate_model_id: GateModelId,
    pub(crate) envelope: CostEnvelope,
    pub(crate) registry: Arc<HookRegistry>,
    pub(crate) injection_filters: FilterRegistry,
    pub(crate) memory: Option<Arc<dyn MemoryRuntime + Send + Sync>>,
    pub(crate) memory_recall_k: usize,
    pub(crate) memory_recall_threshold: f32,
    pub(crate) journal: Option<Arc<dyn SessionJournal>>,
    pub(crate) chain_tail: Mutex<Option<Sha256Digest>>,
    /// Serializes receipt signing + journal/receipt persistence so concurrent
    /// turns cannot fork the receipt hash chain or roll back each other's
    /// journal entries.
    pub(crate) commit_lock: AsyncMutex<()>,
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
    /// ARD-491 — the per-turn cap (bytes) on accumulated streamed assistant
    /// content.
    pub(crate) stream_content_max_bytes: usize,
    /// ARD-139 — the shared on-disk approval-card store a gated tool call
    /// proposes into. `None` (the builder default) disables approval-gating
    /// entirely, regardless of [`approval_gated_capabilities`], so a runtime
    /// that does not opt in behaves exactly as before this stage existed.
    ///
    /// [`approval_gated_capabilities`]: Self::approval_gated_capabilities
    pub(crate) approvals: Option<ApprovalStore>,
    /// ARD-139 — the [`Capability`] labels that require human approval before
    /// a tool call carrying them may proceed, even once the cap-token/cedar
    /// checks already allow it. Empty (the builder default) gates nothing.
    pub(crate) approval_gated_capabilities: HashSet<String>,
    /// #502 Seam B7 (Phase 1) — the opt-in governance emitter mirrored at the
    /// commit decision. `None` (the builder default) leaves the turn path
    /// untouched; `Some` receives only committed rounds, under the commit
    /// lock, immediately after the native receipt append.
    pub(crate) governance: Option<Arc<dyn GovernanceEmitter>>,
}

/// Which cost predicate a control-plane receipt verifies under.
///
/// gh#470 R2 + review round 2: the relaxation to cost 0 is correct ONLY
/// for approval decisions, whose HTTP gate admits them at cost 0. It
/// must not silently widen to checkpoint/rollback/task-control paths.
#[derive(Clone, Copy)]
pub enum ControlVerifyCost {
    /// `approval.decide`: mirror the HTTP admin gate's cost-0 predicate.
    ApprovalDecision,
    /// Every other control operation: the runtime's `cost_units`.
    RuntimeDefault,
}

/// The single execution an `Approved` card authorises, won by THIS call
/// (gh#497). Carried from the approval gate to the tool-invocation site so
/// the invocation's outcome can be recorded on the spent card; the claim
/// itself (consume-before-invoke) is what authorises the call.
#[derive(Clone, Debug)]
pub(crate) struct SpentApproval {
    /// The claimed card's id.
    approval_id: String,
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
        self.verify_cap_token_for_tool_at(cap_token, tool, self.cost_units)
    }

    /// Verify `cap_token` for `tool` at an explicit cost.
    ///
    /// gh#470 R2: the server's admin gate verifies `approval.decide` at
    /// cost 0 because admin mutations are not metered against a turn
    /// budget — so the approval-decision receipt path mirrors that COST
    /// (see [`ControlVerifyCost::ApprovalDecision`]). Cost equality is
    /// what matters here; the two paths still differ deliberately in
    /// their timestamps (the gate verifies at request time, the runtime
    /// at receipt time), while both consult the SAME shared deny backend
    /// since E4.1 — a revocation is honored at admission and again here.
    fn verify_cap_token_for_tool_at(
        &self,
        cap_token: &CapTokenRef,
        tool: &str,
        cost_units: u64,
    ) -> Result<VerifiedClaims, RuntimeError> {
        self.stage_cap_token_for_tool_at(
            &SubmitRequest {
                messages: Vec::new(),
                cap_token: cap_token.clone(),
                session_id: SessionId::new(),
                requested_provider: None,
            },
            &PerRequestProvisioning::default(),
            self.clock.now_ms().get() / 1000,
            tool,
            cost_units,
        )
    }

    /// **gh#533.** Authorize a paid control-plane provider call (compaction
    /// preview/apply, background task) exactly the way a chat turn is
    /// authorized: stage-1 cap-token verification against `tool`, then a
    /// stage-2 Cedar decision under a control-specific action — no parallel
    /// guard stack, the same [`stage_cap_token_for_tool_at`] /
    /// [`stage_cedar_with_action`] helpers [`submit`](ChatRuntime::submit)
    /// uses. A missing policy (the fail-closed deny-all default) and a
    /// matching `forbid` both surface as [`RuntimeError::PolicyDenied`]
    /// BEFORE any budget is reserved or the provider is reached.
    ///
    /// The action is deliberately NOT `Action::Submit`: an operator policy
    /// that permits chat but withholds compaction (a paid meta-operation over
    /// the transcript) must be expressible without denying ordinary turns,
    /// and vice versa. The cap-token's tool caveat still scopes the call to
    /// the control capability (`context.compact`, `task.background`), so both
    /// gates the chat path applies are present here, in the same order.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] when the cap-token is missing/expired/
    /// revoked/malformed, does not grant `tool`, or the Cedar decision is not
    /// `Allow`.
    fn authorize_control_provider_call(
        &self,
        session_id: SessionId,
        cap_token: &CapTokenRef,
        tool: &str,
        action: ActionRef,
    ) -> Result<VerifiedClaims, RuntimeError> {
        let claims = self.verify_cap_token_for_tool_at(cap_token, tool, self.cost_units)?;
        self.stage_cedar_with_action(session_id, &claims, action)?;
        Ok(claims)
    }

    /// **gh#533.** Run one paid control-plane provider call through the SAME
    /// economic admission a chat turn takes: a supervised settlement turn, a
    /// cost-gate reservation of the projected envelope BEFORE dispatch,
    /// provider evidence observation, and a truthful settlement afterwards —
    /// a signed, chained receipt carrying the provider-reported cost on
    /// success, an infrastructure-failure refund on a provider error. The
    /// caller owns no reservation on either exit path.
    ///
    /// Unknown price is not free: the reservation is taken against the
    /// projected envelope (the operator-configured worst case), and the
    /// settled debit is the observed cost when the provider reports one —
    /// so an unpriced response still consumed real admission authority, and
    /// a priced one settles at what was actually billed, mirroring
    /// [`submit`](ChatRuntime::submit)'s admit → observe → settle sequence.
    ///
    /// Returns the completion response plus the minted receipt body (the
    /// durable audit record of the external send).
    ///
    /// # Errors
    /// Returns [`RuntimeError`] when admission is refused
    /// ([`RuntimeError::CostCeilingExceeded`] for an exhausted budget), the
    /// provider call fails (after the failure refund), or the receipt cannot
    /// be durably minted — in which case the turn is marked unresolved and
    /// the error surfaces, never a silent success.
    async fn dispatch_control_completion(
        &self,
        session_id: SessionId,
        claims: &VerifiedClaims,
        request: CompletionRequest,
        verb: &'static str,
    ) -> Result<(CompletionResponse, ReceiptBody), RuntimeError> {
        // Waiting control callers own no settlement capacity: declare the
        // economic permit before the settlement owner, as submit does.
        let mut economic_permit = Some(self.economic_admission.lock().await);
        let mut owner =
            self.begin_settlement(session_id, claims, &PerRequestProvisioning::default())?;
        let gate_token_id = GateTokenId(claims.token_id);
        self.gate
            .bind_token(gate_token_id, GateHolderId(claims.subject.0.clone()));

        let request_digest = GateSha256::of(
            &serde_json::to_vec(&request).map_err(|e| RuntimeError::Internal(e.into()))?,
        );
        let reservation = self
            .admit_round(
                &mut owner,
                AdmissionRequest {
                    cap_token_id: gate_token_id,
                    projected_envelope: self.envelope,
                    provider_id: self.gate_provider_id.clone(),
                    model_id: self.gate_model_id.clone(),
                    request_digest,
                },
                request.request_id.0,
            )
            .await?;

        reservation
            .owner
            .observe_provider(ProviderEvidence::DispatchIntent)
            .map_err(settlement_error)?;
        let provider_result = self.provider.complete(request).await;
        let response = match provider_result {
            Ok(response) => response,
            Err(provider_err) => {
                // Provider failure: refund the reserved envelope as an
                // infrastructure failure and surface the provider's error
                // through the SAME typed mapping the chat path uses — a
                // provider-reported CostCeilingExceeded stays
                // CostCeilingExceeded (a hard error for callers like
                // run_background_task, not an invariant-12 failed task),
                // everything else is the canonical ProviderUnavailable
                // dispatch-failure signal.
                let mapped = map_provider_error(&provider_err);
                self.release_failure(reservation).await;
                return Err(mapped);
            }
        };
        reservation
            .owner
            .observe_provider(ProviderEvidence::Observed {
                usage: Some(UsageSnapshot {
                    input_tokens: u64::from(response.usage.tokens_in),
                    output_tokens: u64::from(response.usage.tokens_out),
                }),
                cost: response.cost,
                provenance: CostProvenance::ResponseCost,
                finished: true,
                interrupted: false,
            })
            .map_err(settlement_error)?;
        self.gate.touch_reservation(reservation.reservation_id);

        // Settle with a receipt, under the same commit lock + append contract
        // commit_round uses, so the control receipt chains onto the turn
        // chain and a persistence failure is recorded, never swallowed.
        let verb = VerbObject::new(verb)
            .map_err(|e| RuntimeError::Internal(anyhow::anyhow!("invalid receipt verb: {e}")))?;
        let now_ms = self.clock.now_ms().get();
        let receipt = {
            let _guard = self.commit_lock.lock().await;
            let parent_hash = *self.chain_tail.lock();
            let body = ReceiptBody {
                receipt_id: uuid::Uuid::new_v4(),
                parent_hash,
                verb,
                issued_at: ardur_receipt::UnixTsMillis(now_ms),
                subject: ardur_receipt::HolderId(claims.subject.0.clone()),
                cap_token_id: ardur_receipt::TokenId(claims.token_id),
                payload_digest: Sha256Digest::of(response.content.as_bytes()),
                session_id: Some(session_id.0),
                cost: response.cost,
                tool_calls: Vec::new(),
                provider: Some(self.provider.name()),
            };
            // gh#533 review: every failure from here on is POST-dispatch —
            // the send happened and its cost is observed — so each must
            // terminate the settlement as unresolved (stopping new
            // admission), never as a clean cancellation via a dropped
            // reservation.
            let signed = match ReceiptSigner::sign(body, &self.receipt_key) {
                Ok(signed) => signed,
                Err(e) => {
                    self.settlements.stop_admission();
                    reservation
                        .owner
                        .receipt_unresolved()
                        .map_err(settlement_error)?;
                    return Err(RuntimeError::Internal(anyhow::anyhow!(
                        "receipt mint failed: {e}"
                    )));
                }
            };
            let path = self
                .receipt_log
                .as_ref()
                .expect("builder requires receipt storage");
            let expected_log_end =
                match crate::receipts::open_append_no_follow(path).and_then(|f| f.metadata()) {
                    Ok(meta) => meta.len(),
                    Err(e) => {
                        self.settlements.stop_admission();
                        reservation
                            .owner
                            .receipt_unresolved()
                            .map_err(settlement_error)?;
                        return Err(RuntimeError::Internal(anyhow::anyhow!(
                            "receipt log unreadable after dispatch: {e}"
                        )));
                    }
                };
            reservation
                .owner
                .prepare_receipt(
                    ReceiptCandidate {
                        receipt_id: ReceiptId(signed.body().receipt_id),
                        expected_parent: signed.body().parent_hash,
                        expected_log_end,
                        jws_compact: signed.jws_compact().to_string(),
                        jws_digest: GateSha256::of(signed.jws_compact().as_bytes()),
                    },
                    true,
                    signed.body().cost,
                )
                .map_err(settlement_error)?;
            reservation
                .owner
                .finalize_sync()
                .map_err(settlement_error)?;
            if let Err(e) = crate::receipts::append_at_expected_end(
                path,
                expected_log_end,
                signed.jws_compact(),
            ) {
                self.settlements.stop_admission();
                reservation
                    .owner
                    .receipt_unresolved()
                    .map_err(settlement_error)?;
                return Err(RuntimeError::Internal(anyhow::anyhow!(
                    "settlement receipt unresolved: {e}"
                )));
            }
            *self.chain_tail.lock() = Some(Sha256Digest::of(signed.jws_compact().as_bytes()));
            reservation
                .owner
                .commit_receipt(ReceiptBinding {
                    receipt_id: ReceiptId(signed.body().receipt_id),
                    jws_digest: GateSha256::of(signed.jws_compact().as_bytes()),
                })
                .map_err(settlement_error)?;
            signed.body().clone()
        };
        drop(economic_permit.take());
        self.drain_pending_settlements()
            .await
            .map_err(settlement_error)?;
        Ok((response, receipt))
    }

    /// The journal this runtime is wired to, or a typed error if none is
    /// configured — every §1.7/§1.8/§1.9 session-control operation requires a
    /// durable journal to record its state transition against.
    fn journal_or_err(&self) -> Result<&Arc<dyn SessionJournal>, RuntimeError> {
        self.journal.as_ref().ok_or_else(|| {
            RuntimeError::Internal(anyhow::anyhow!(
                "no session journal is configured for this runtime"
            ))
        })
    }

    /// **§1.8.** Record a checkpoint: a named resume point over the session's
    /// current history, with a signed receipt chained onto the same receipt
    /// chain turns use. Read-only — nothing about the session's live history
    /// changes; a checkpoint is purely a marker later [`rollback_to_checkpoint`](Self::rollback_to_checkpoint)
    /// or [`SessionJournal::replay_from`] can target.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the cap-token does not grant `tool`, if no
    /// journal is configured, or if the journal append fails.
    pub async fn checkpoint(
        &self,
        session_id: SessionId,
        cap_token: &CapTokenRef,
        tool: &str,
        label: Option<String>,
    ) -> Result<CheckpointOutcome, RuntimeError> {
        let journal = self.journal_or_err()?;
        let entry_count = journal
            .replay(session_id)
            .await
            .map_err(|e| RuntimeError::Internal(anyhow::anyhow!("journal replay failed: {e}")))?
            .len();
        let checkpoint_id = uuid::Uuid::new_v4();
        let summary = label.unwrap_or_else(|| format!("checkpoint at {entry_count} entries"));

        let receipt = self
            .commit_control_receipt(
                session_id,
                cap_token,
                tool,
                "session.checkpoint.created.v1",
                Sha256Digest::of(summary.as_bytes()),
                ardur_receipt::CostTuple {
                    tokens_in: 0,
                    tokens_out: 0,
                    cents: 0,
                    wall_ms: 0,
                    attention_score: 0,
                },
                ControlVerifyCost::RuntimeDefault,
            )
            .await?;

        let entry_id = journal
            .append(JournalEntry::Checkpoint {
                checkpoint_id,
                summary: summary.clone(),
                at: self.clock.now_ms(),
            })
            .await
            .map_err(|e| RuntimeError::Internal(anyhow::anyhow!("journal append failed: {e}")))?;

        Ok(CheckpointOutcome {
            checkpoint_id,
            entry_id,
            receipt_id: ReceiptId(receipt.receipt_id),
            summary,
        })
    }

    /// **§1.8.** List every checkpoint recorded in this session's journal, in
    /// creation order. Read-only: no receipt is minted for a query.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if no journal is configured or the replay
    /// fails.
    pub async fn list_checkpoints(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<CheckpointInfo>, RuntimeError> {
        let journal = self.journal_or_err()?;
        let entries = journal
            .replay(session_id)
            .await
            .map_err(|e| RuntimeError::Internal(anyhow::anyhow!("journal replay failed: {e}")))?;
        Ok(entries
            .iter()
            .enumerate()
            .filter_map(|(pos, entry)| match entry {
                JournalEntry::Checkpoint {
                    checkpoint_id,
                    summary,
                    at,
                } => Some(CheckpointInfo {
                    checkpoint_id: *checkpoint_id,
                    entry_id: ardur_session_journals::EntryId::new(pos as u64),
                    summary: summary.clone(),
                    at: at.get(),
                }),
                _ => None,
            })
            .collect())
    }

    /// **§1.8.** Roll back to a previously recorded checkpoint: append a
    /// [`JournalEntry::Rollback`] marker (the journal stays append-only —
    /// nothing between the checkpoint and this marker is deleted or
    /// rewritten, only excluded from the *live* reconstruction going
    /// forward) and mint a chained receipt recording the rollback.
    ///
    /// Returns the full entry log up to and including the target checkpoint
    /// so the caller can rebuild its in-memory session state without a
    /// second journal round-trip.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the cap-token does not grant `tool`, no
    /// journal is configured, `checkpoint_id` does not name a checkpoint in
    /// this session's journal, or the journal append fails.
    pub async fn rollback_to_checkpoint(
        &self,
        session_id: SessionId,
        cap_token: &CapTokenRef,
        tool: &str,
        checkpoint_id: uuid::Uuid,
    ) -> Result<RollbackOutcome, RuntimeError> {
        let journal = self.journal_or_err()?;
        let entries = journal
            .replay(session_id)
            .await
            .map_err(|e| RuntimeError::Internal(anyhow::anyhow!("journal replay failed: {e}")))?;
        let checkpoint_pos = entries
            .iter()
            .position(|entry| {
                matches!(entry, JournalEntry::Checkpoint { checkpoint_id: id, .. } if *id == checkpoint_id)
            })
            .ok_or_else(|| {
                RuntimeError::Internal(anyhow::anyhow!(
                    "checkpoint {checkpoint_id} not found in this session's journal"
                ))
            })?;

        let receipt = self
            .commit_control_receipt(
                session_id,
                cap_token,
                tool,
                "session.rollback.completed.v1",
                Sha256Digest::of(checkpoint_id.as_bytes()),
                ardur_receipt::CostTuple {
                    tokens_in: 0,
                    tokens_out: 0,
                    cents: 0,
                    wall_ms: 0,
                    attention_score: 0,
                },
                ControlVerifyCost::RuntimeDefault,
            )
            .await?;
        let receipt_id = ReceiptId(receipt.receipt_id);

        let entry_id = journal
            .append(JournalEntry::Rollback {
                target_checkpoint_id: checkpoint_id,
                receipt_id,
                at: self.clock.now_ms(),
            })
            .await
            .map_err(|e| RuntimeError::Internal(anyhow::anyhow!("journal append failed: {e}")))?;

        Ok(RollbackOutcome {
            target_checkpoint_id: checkpoint_id,
            entry_id,
            receipt_id,
            retained_entries: entries[..=checkpoint_pos].to_vec(),
        })
    }

    /// **§1.7.** Build the provider request that summarizes `history` into a
    /// structured compaction summary. gh#533: returning the *request* (rather
    /// than dispatching it here) lets the admission layer bind the cost-gate
    /// reservation to the exact serialized request (its SHA-256 digest), the
    /// same request-binding the chat turn's admission uses. Bypasses the turn
    /// pipeline (`submit`/`stream`) deliberately: this is a meta-operation
    /// *over* the transcript, not a chat turn, so it must not itself become a
    /// journaled `UserMessage`/`AssistantMessage` pair the way a real turn
    /// would — that would pollute the conversation it is trying to summarize.
    ///
    /// A condensed practical subset of the blueprint's nine-heading summary
    /// template: Active Task, Completed Actions, Open Items, Decisions,
    /// Critical Exact Values, Next Best Step. The full template's
    /// Mission/Policy/Memory-anchor sections need substrate (a live mission
    /// object, capability-grant tracking, a memory index) this runtime does
    /// not yet expose to a summarization call, so they are omitted rather
    /// than filled with placeholders.
    fn summarize_request(&self, history: &[ChatMessage], focus: Option<&str>) -> CompletionRequest {
        let mut instruction = String::from(
            "Summarize the conversation below for continuation by another AI \
             agent. Use exactly this structure:\n\n\
             ## Active Task\n[the most recent unfulfilled user request, exact wording where possible]\n\n\
             ## Completed Actions\n[concrete actions taken, with outcomes, commands, file paths, test results]\n\n\
             ## Open Items\n[pending tasks, blockers, unanswered questions]\n\n\
             ## Decisions\n[decisions made and why]\n\n\
             ## Critical Exact Values\n[paths, IDs, error strings, hashes, names, ports — redact any secrets]\n\n\
             ## Next Best Step\n[one concise statement of what to do next]\n\n\
             Be concise. Omit a section entirely if it has nothing to report.",
        );
        if let Some(focus) = focus {
            instruction.push_str(&format!(
                "\n\nPrioritize preserving detail related to: {focus}"
            ));
        }
        let mut messages = vec![ChatMessage::system(instruction)];
        messages.extend_from_slice(history);

        CompletionRequest::new(messages, self.model.clone(), self.max_tokens)
    }

    /// **§1.7.** Summarize `history` and install the result as a compaction
    /// checkpoint: mints a control receipt recording the real token/cost the
    /// summarization call incurred, and records the summary as a
    /// [`JournalEntry::Checkpoint`] so the existing
    /// [`rollback_to_checkpoint`](Self::rollback_to_checkpoint) machinery can
    /// restore it later — a compaction checkpoint and a manual `/checkpoint`
    /// are the same underlying journal record; only how the summary text was
    /// produced differs.
    ///
    /// **gh#533:** the paid summarization call is admitted BEFORE dispatch
    /// through the same two gates a chat turn passes — Cedar authorization
    /// (action `Action::ContextCompact`, after cap-token verification for
    /// `tool`) and a cost-gate reservation against the holder's budget. A
    /// missing/forbid policy denies with [`RuntimeError::PolicyDenied`] and
    /// an exhausted budget denies with [`RuntimeError::CostCeilingExceeded`],
    /// in both cases without reaching the provider. Settlement is truthful:
    /// the reservation is released as an infrastructure failure when the
    /// provider errors, and the minted receipt settles at the observed cost.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the cap-token does not grant `tool`, Cedar
    /// denies the action, the budget cannot cover the envelope, the provider
    /// call fails, no journal is configured, or the journal append fails.
    pub async fn compact(
        &self,
        session_id: SessionId,
        cap_token: &CapTokenRef,
        tool: &str,
        history: &[ChatMessage],
        focus: Option<String>,
    ) -> Result<CompactOutcome, RuntimeError> {
        // gh#533: full chat-parity admission — cap-token + Cedar + cost gate
        // — BEFORE the provider is reached. A denial here spends nothing.
        let claims = self.authorize_control_provider_call(
            session_id,
            cap_token,
            tool,
            ActionRef("Action::ContextCompact".to_string()),
        )?;
        let journal = self.journal_or_err()?;

        let before_tokens = estimate_tokens(history);
        let request = self.summarize_request(history, focus.as_deref());
        let (response, receipt) = self
            .dispatch_control_completion(session_id, &claims, request, COMPACT_APPLIED_VERB)
            .await?;
        let after_tokens = estimate_tokens(std::slice::from_ref(&ChatMessage::assistant(
            response.content.clone(),
        )));

        let checkpoint_id = uuid::Uuid::new_v4();
        let entry_id = journal
            .append(JournalEntry::Checkpoint {
                checkpoint_id,
                summary: response.content.clone(),
                at: self.clock.now_ms(),
            })
            .await
            .map_err(|e| RuntimeError::Internal(anyhow::anyhow!("journal append failed: {e}")))?;

        Ok(CompactOutcome {
            checkpoint_id,
            entry_id,
            receipt_id: ReceiptId(receipt.receipt_id),
            summary: response.content,
            before_tokens_estimate: before_tokens,
            after_tokens_estimate: after_tokens,
        })
    }

    /// **§1.7.** Summarize `history` without installing it: no journal entry,
    /// no session state change — lets a caller preview a compaction candidate
    /// before committing to it with [`compact`](Self::compact).
    ///
    /// **gh#533 audit contract:** preview is non-mutating to the
    /// conversation, but it IS an external provider send — the full prompt
    /// transcript leaves the process exactly as an applied compaction does.
    /// It therefore passes the SAME admission as `compact` (cap-token +
    /// Cedar `Action::ContextCompact` + cost reservation) and mints a
    /// `context.compact.previewed.v1` receipt carrying the observed cost, so
    /// the send is durably auditable. A receipt-persistence failure is an
    /// `Err` (the settlement is marked unresolved), never a silent
    /// success-without-audit: the runtime must not claim no egress happened
    /// because persistence failed.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the cap-token does not grant `tool`, Cedar
    /// denies the action, the budget cannot cover the envelope, the provider
    /// call fails, or the audit receipt cannot be durably minted.
    pub async fn preview_compact(
        &self,
        session_id: SessionId,
        cap_token: &CapTokenRef,
        tool: &str,
        history: &[ChatMessage],
        focus: Option<String>,
    ) -> Result<String, RuntimeError> {
        let claims = self.authorize_control_provider_call(
            session_id,
            cap_token,
            tool,
            ActionRef("Action::ContextCompact".to_string()),
        )?;
        let request = self.summarize_request(history, focus.as_deref());
        let (response, _receipt) = self
            .dispatch_control_completion(session_id, &claims, request, COMPACT_PREVIEWED_VERB)
            .await?;
        Ok(response.content)
    }

    /// **§1.9.** Run one agent background task: a single prompt dispatched
    /// straight to the provider (like [`compact`](Self::compact), this
    /// bypasses `submit`/`stream` — a background task's own transcript is
    /// not the foreground conversation, per the blueprint's invariant that a
    /// background task "can be inspected without injecting its full context
    /// into the foreground chat").
    ///
    /// Unlike `compact`, a provider failure here is *not* an `Err` — the
    /// blueprint's invariant 12 requires a terminal receipt whether the task
    /// completes or fails, so a failed provider call still mints a
    /// `task.background.failed.v1` receipt and returns `Ok` with the outcome's
    /// `error` field set. Only a denied cap-token (never even attempted) or a
    /// receipt-mint failure is a hard `Err`.
    ///
    /// **gh#533:** the provider call is admitted BEFORE dispatch through the
    /// same two gates a chat turn passes — Cedar authorization (action
    /// `Action::TaskBackground`, after cap-token verification for `tool`) and
    /// a cost-gate reservation. A policy denial or an exhausted budget is a
    /// hard `Err` BEFORE the provider is reached, and the failure receipt
    /// path settles truthfully: the reserved envelope is refunded as an
    /// infrastructure failure, then the zero-cost terminal
    /// `task.background.failed.v1` marker is minted so the chain records the
    /// task's outcome without double-counting the refund.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the cap-token does not grant `tool`, Cedar
    /// denies the action, the budget cannot cover the envelope, or a receipt
    /// could not be minted.
    pub async fn run_background_task(
        &self,
        session_id: SessionId,
        cap_token: &CapTokenRef,
        tool: &str,
        prompt: &str,
    ) -> Result<BackgroundTaskOutcome, RuntimeError> {
        // gh#533: full chat-parity admission BEFORE the provider is reached.
        let claims = self.authorize_control_provider_call(
            session_id,
            cap_token,
            tool,
            ActionRef("Action::TaskBackground".to_string()),
        )?;
        let req = CompletionRequest::new(
            vec![ChatMessage::user(prompt)],
            self.model.clone(),
            self.max_tokens,
        );
        match self
            .dispatch_control_completion(session_id, &claims, req, TASK_COMPLETED_VERB)
            .await
        {
            Ok((response, receipt)) => Ok(BackgroundTaskOutcome {
                receipt_id: ReceiptId(receipt.receipt_id),
                result: Some(response.content),
                error: None,
            }),
            Err(dispatch_err) => {
                // The dispatch already released its reservation (as an
                // infrastructure-failure refund on a provider error, or it
                // never took one on an admission refusal). Surface
                // admission refusals and audit failures as the hard error
                // they are; only a *provider* failure converts to the
                // invariant-12 terminal failed-task receipt.
                let message = dispatch_err.to_string();
                if !matches!(dispatch_err, RuntimeError::ProviderUnavailable) {
                    return Err(dispatch_err);
                }
                let receipt = self
                    .commit_control_receipt(
                        session_id,
                        cap_token,
                        tool,
                        TASK_FAILED_VERB,
                        Sha256Digest::of(message.as_bytes()),
                        ardur_receipt::CostTuple {
                            tokens_in: 0,
                            tokens_out: 0,
                            cents: 0,
                            wall_ms: 0,
                            attention_score: 0,
                        },
                        ControlVerifyCost::RuntimeDefault,
                    )
                    .await?;
                Ok(BackgroundTaskOutcome {
                    receipt_id: ReceiptId(receipt.receipt_id),
                    result: None,
                    error: Some(message),
                })
            }
        }
    }

    /// **§1.9.** Mint the terminal receipt for a background task cancelled by
    /// explicit user action (invariant 12: a background task must leave a
    /// terminal receipt whether it completes, fails, times out, is
    /// cancelled, or becomes lost — this MVP does not yet implement
    /// timeout/lost detection).
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the cap-token does not grant `tool` or the
    /// receipt could not be minted.
    pub async fn cancel_background_task(
        &self,
        session_id: SessionId,
        cap_token: &CapTokenRef,
        tool: &str,
    ) -> Result<ReceiptId, RuntimeError> {
        let receipt = self
            .commit_control_receipt(
                session_id,
                cap_token,
                tool,
                "task.background.cancelled.v1",
                Sha256Digest::of(b"cancelled by user"),
                ardur_receipt::CostTuple {
                    tokens_in: 0,
                    tokens_out: 0,
                    cents: 0,
                    wall_ms: 0,
                    attention_score: 0,
                },
                ControlVerifyCost::RuntimeDefault,
            )
            .await?;
        Ok(ReceiptId(receipt.receipt_id))
    }

    /// **§1.10.** Mint the receipt for a steering directive accepted against
    /// a target background task (verb `input.steer.accepted.v1`).
    ///
    /// KNOWN LIMITATION: this MVP's background-task runtime
    /// ([`run_background_task`](Self::run_background_task)) is a single
    /// one-shot provider call with no iterative loop to check a steering
    /// queue between iterations — the same way `/compact`'s summarizer or a
    /// chat turn's provider round is a single call. So a steer directive is
    /// durably recorded and receipted (real evidence a steering request was
    /// made and accepted) but does **not** yet change the target task's
    /// in-flight behavior. It becomes actionable once a task type actually
    /// loops (task flows, §1.9's deferred scope). Surfaced, not hidden: the
    /// CLI's `/steer` response says this explicitly.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the cap-token does not grant `tool` or
    /// the receipt could not be minted.
    pub async fn accept_steer_directive(
        &self,
        session_id: SessionId,
        cap_token: &CapTokenRef,
        tool: &str,
        target_task_id: uuid::Uuid,
        message: &str,
    ) -> Result<ReceiptId, RuntimeError> {
        let receipt = self
            .commit_control_receipt(
                session_id,
                cap_token,
                tool,
                "input.steer.accepted.v1",
                Sha256Digest::of(format!("{target_task_id}:{message}").as_bytes()),
                ardur_receipt::CostTuple {
                    tokens_in: 0,
                    tokens_out: 0,
                    cents: 0,
                    wall_ms: 0,
                    attention_score: 0,
                },
                ControlVerifyCost::RuntimeDefault,
            )
            .await?;
        Ok(ReceiptId(receipt.receipt_id))
    }

    /// **§1.10.** Mint the receipt for an accepted interrupt against a
    /// target background task (verb `input.interrupt.accepted.v1`) —
    /// distinct from [`cancel_background_task`](Self::cancel_background_task)'s
    /// `task.background.cancelled.v1` even though both end the same task:
    /// the blueprint models "the user interrupted the active run" and "the
    /// user cancelled a background task" as different intents worth
    /// distinguishing in the receipt trail, even when today's MVP resolves
    /// both the same mechanical way (abort the task).
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the cap-token does not grant `tool` or
    /// the receipt could not be minted.
    pub async fn accept_interrupt(
        &self,
        session_id: SessionId,
        cap_token: &CapTokenRef,
        tool: &str,
        target_task_id: uuid::Uuid,
    ) -> Result<ReceiptId, RuntimeError> {
        let receipt = self
            .commit_control_receipt(
                session_id,
                cap_token,
                tool,
                "input.interrupt.accepted.v1",
                Sha256Digest::of(target_task_id.as_bytes()),
                ardur_receipt::CostTuple {
                    tokens_in: 0,
                    tokens_out: 0,
                    cents: 0,
                    wall_ms: 0,
                    attention_score: 0,
                },
                ControlVerifyCost::RuntimeDefault,
            )
            .await?;
        Ok(ReceiptId(receipt.receipt_id))
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
        self.deny.revoke_token(&token).map_err(|e| {
            // Do not acknowledge unconfirmed persistence (gh#361): append or
            // sync errors may follow a partial write, so durability is unknown.
            RuntimeError::Internal(anyhow::anyhow!("persisting revocation failed: {e}"))
        })?;
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

    /// Retain this handle outside the runtime until pending work is drained or
    /// explicitly quarantined. Dropping it does not spawn cleanup work.
    pub fn settlement_supervisor(&self) -> SettlementSupervisor {
        self.settlements.supervisor()
    }

    /// Bounded asynchronous projection boundary, including synchronous Drop's
    /// operator expense. An ambiguous journal attempt remains fail-closed.
    pub async fn drain_pending_settlements(&self) -> Result<usize, SettlementError> {
        match &self.journal {
            Some(journal) => {
                self.settlement_supervisor()
                    .drain_pending(journal.as_ref())
                    .await
            }
            None => Ok(0),
        }
    }

    fn begin_settlement(
        &self,
        session_id: SessionId,
        claims: &VerifiedClaims,
        provisioning: &PerRequestProvisioning,
    ) -> Result<TurnSettlementOwner, RuntimeError> {
        self.settlements
            .reserve_turn(TurnIdentity {
                request_session: session_id,
                journal_owner: self.journal.as_ref().map(|j| *j.session_id()),
                verified_subject: GateHolderId(claims.subject.0.clone()),
                budget_holder: provisioning
                    .subject
                    .clone()
                    .unwrap_or_else(|| GateHolderId(claims.subject.0.clone())),
                cap_token_id: GateTokenId(claims.token_id),
                started_at: self.clock.now_ms(),
            })
            .map_err(settlement_error)
    }

    async fn admit_round<'a>(
        &self,
        owner: &'a mut TurnSettlementOwner,
        request: AdmissionRequest,
        provider_request_id: uuid::Uuid,
    ) -> Result<RuntimeReservation<'a>, RuntimeError> {
        owner
            .admit_round(request, provider_request_id)
            .await
            .map_err(settlement_error)?;
        let reservation_id = owner.reservation_id().map_err(settlement_error)?;
        Ok(RuntimeReservation {
            owner,
            reservation_id,
        })
    }

    async fn release(&self, reservation: RuntimeReservation<'_>) {
        let result = reservation.owner.abandon_sync();
        if let Err(error) = result {
            tracing::error!(%error, "settlement cancellation retained for supervision");
        }
        if let Err(error) = self.drain_pending_settlements().await {
            tracing::error!(%error, "settlement projection pending");
        }
    }

    async fn release_failure(&self, reservation: RuntimeReservation<'_>) {
        if let Err(error) = reservation
            .owner
            .refund_failure(InfrastructureFailureClass::Provider)
        {
            tracing::error!(%error, "settlement failure refund retained for supervision");
        }
        if let Err(error) = self.drain_pending_settlements().await {
            tracing::error!(%error, "settlement failure projection pending");
        }
    }

    async fn settle_refusal(
        &self,
        _session_id: SessionId,
        reservation: RuntimeReservation<'_>,
        known: RuntimeCostTuple,
        reason: &str,
    ) -> Result<(), RuntimeError> {
        let decision = match reason {
            UNKNOWN_TOOL_SETTLEMENT => Disposition::Refusal(RefusalClass::UnknownTool),
            TOOL_CAPACITY_SETTLEMENT => Disposition::Refusal(RefusalClass::Capacity),
            TOOL_AUTH_SETTLEMENT => Disposition::Refusal(RefusalClass::Authorization),
            CAPABILITY_SETTLEMENT => Disposition::Refusal(RefusalClass::MissingCapability),
            APPROVAL_SETTLEMENT => Disposition::Refusal(RefusalClass::ApprovalRequired),
            APPROVAL_REJECTED_SETTLEMENT => Disposition::Refusal(RefusalClass::ApprovalRejected),
            SCAN_SETTLEMENT => Disposition::Refusal(RefusalClass::OutputBlocked),
            _ => Disposition::Infrastructure(InfrastructureFailureClass::Provider),
        };
        reservation
            .owner
            .prepare(decision, known)
            .map_err(settlement_error)?;
        reservation
            .owner
            .finalize_sync()
            .map_err(settlement_error)?;
        let projection = self.drain_pending_settlements().await;
        if matches!(projection, Err(SettlementError::ProjectionNotApplied)) {
            self.settlement_supervisor()
                .compensate_definite_failure(reservation.owner.turn_id())
                .map_err(settlement_error)?;
            return Err(settlement_error(SettlementError::ProjectionNotApplied));
        }
        projection.map_err(settlement_error)?;
        reservation
            .owner
            .commit_disposition()
            .map_err(settlement_error)
    }

    fn observe_usage(
        &self,
        reservation: &mut RuntimeReservation<'_>,
        usage: Usage,
        finished: bool,
    ) -> Result<(), RuntimeError> {
        let rate = self.provider.rate_card();
        let rate_bytes = serde_json::to_vec(rate).map_err(|e| RuntimeError::Internal(e.into()))?;
        reservation
            .owner
            .observe_provider(ProviderEvidence::Observed {
                usage: Some(UsageSnapshot {
                    input_tokens: u64::from(usage.tokens_in),
                    output_tokens: u64::from(usage.tokens_out),
                }),
                cost: rate.price(usage),
                provenance: CostProvenance::PricedUsage(GateSha256::of(&rate_bytes)),
                finished,
                interrupted: false,
            })
            .map_err(settlement_error)
    }

    fn observe_tool(
        &self,
        reservation: &mut RuntimeReservation<'_>,
        ordinal: usize,
        call: &ToolCall,
        effect: ToolEffect,
        admission: OutputAdmission,
    ) -> Result<(), RuntimeError> {
        reservation
            .owner
            .observe_tool(ToolEvidence {
                ordinal: ordinal as u32,
                call_id: call.id.clone(),
                name: call.name.clone(),
                arguments_digest: GateSha256::of(
                    &serde_json::to_vec(&call.arguments)
                        .map_err(|e| RuntimeError::Internal(e.into()))?,
                ),
                effect,
                output_admission: admission,
            })
            .map_err(settlement_error)
    }

    /// The honest event outcome for a tool error: the typed refusal variants
    /// are requests refused BEFORE the effect (a shell command outside its
    /// allowlist, a path escaping the tool root, a missing capability grant,
    /// a rejected token, malformed arguments, a cost ceiling) and must be
    /// recorded as canonical denials — projecting them as `FailedUnknown`
    /// would report verifier-attributed `insufficient_evidence` for what was
    /// in fact a refusal. `FailedUnknown` is reserved for the variants where
    /// execution may actually have occurred.
    fn tool_error_event_outcome(err: &ToolError) -> EventOutcome {
        use ardur_governance::{DeniedOutcome, PublicDenialReason};
        match err {
            ToolError::Denied { .. } => EventOutcome::Denied(DeniedOutcome {
                public: PublicDenialReason::PolicyDenied,
                internal: "tool_policy_denied".to_string(),
            }),
            ToolError::CapabilityDenied(_) => EventOutcome::Denied(DeniedOutcome {
                public: PublicDenialReason::PolicyDenied,
                internal: "tool_capability_denied".to_string(),
            }),
            ToolError::CapTokenDenied { .. } => EventOutcome::Denied(DeniedOutcome {
                public: PublicDenialReason::PolicyDenied,
                internal: "tool_cap_token_denied".to_string(),
            }),
            ToolError::InvalidArgs(_) => EventOutcome::Denied(DeniedOutcome {
                public: PublicDenialReason::PolicyDenied,
                internal: "tool_invalid_arguments".to_string(),
            }),
            ToolError::CostCeilingExceeded => EventOutcome::Denied(DeniedOutcome {
                public: PublicDenialReason::BudgetExhausted,
                internal: "tool_cost_ceiling_exceeded".to_string(),
            }),
            // Registered but backendless: a deployment gap, not a policy
            // refusal and not an execution — the denial whose own
            // classification is "could not establish" stays insufficient
            // (the projection keeps that mapping).
            ToolError::NotImplemented(_) => EventOutcome::Denied(DeniedOutcome {
                public: PublicDenialReason::InsufficientEvidence,
                internal: "tool_not_implemented".to_string(),
            }),
            // Execution may actually have occurred (the tool ran, or its
            // output was produced but unusable): these stay unknown-effect.
            ToolError::ExecutionFailed(_)
            | ToolError::OutputTooLarge { .. }
            | ToolError::Internal(_) => EventOutcome::FailedUnknown,
            ToolError::Timeout => EventOutcome::TimeoutUnknown,
        }
    }

    /// **#543.** Record the immutable authorization inputs of one evaluated
    /// tool event **before** its effect runs (or at its denial point), so a
    /// post-crash replay can reconstruct the event without re-executing
    /// anything. Best-effort and never gating: when no governance emitter is
    /// configured this is a no-op, and a record failure is logged and skips
    /// the event (its ER stays absent — an honest gap, never a fabricated
    /// fact) rather than affecting the turn.
    #[allow(clippy::too_many_arguments)]
    fn governance_pre_effect(
        &self,
        session_id: SessionId,
        request_id: &str,
        iteration: u32,
        tool_ordinal: usize,
        call: &ToolCall,
        claims: &VerifiedClaims,
        required: &[Capability],
    ) -> Option<PreEffectRecord> {
        let emitter = self.governance.as_ref()?;
        let classification = classify_tool_invocation(&call.name, required);
        let record = PreEffectRecord::tool_invocation(
            &ardur_governance::EventScope {
                session_id: &session_id.0.to_string(),
                request_id,
                iteration,
                tool_ordinal: tool_ordinal as u32,
                call_id: &call.id,
            },
            &call.name,
            &GrantFacts::from(claims),
            &classification,
            call.arguments.clone(),
            self.clock.now_ms().get(),
        );
        match emitter.record_pre_effect(&record) {
            Ok(()) => Some(record),
            Err(err) => {
                tracing::warn!(
                    event_id = %record.event_id,
                    error = %err,
                    "governance pre-effect evidence failed; this event's crash recovery is \
                     degraded to honest absence"
                );
                None
            }
        }
    }

    /// **#543.** Record the terminal observation of one evaluated event and
    /// mirror its ER — the *one ER per evaluated event* the verifier
    /// contract requires. The post-effect observation must land durably
    /// **before** the ER is minted: an ER is only ever projected from durable
    /// evidence, so if the durable post append fails, no ER is minted — the
    /// event reads as honest absence downstream (a restart's sweep will
    /// project the stranded pre record as effect-unobserved), never as an
    /// unsupported verdict. Both operations are best-effort and logged,
    /// never gating.
    fn governance_terminal_event(&self, pre: &Option<PreEffectRecord>, outcome: EventOutcome) {
        let (Some(emitter), Some(pre)) = (self.governance.as_ref(), pre.as_ref()) else {
            return;
        };
        let post = PostEffectRecord::new(pre, self.clock.now_ms().get(), outcome);
        if let Err(err) = emitter.record_post_effect(&post) {
            tracing::warn!(
                event_id = %pre.event_id,
                error = %err,
                "governance post-effect evidence failed; no ER minted for this event — \
                 it reads as honest absence, never as an unsupported verdict"
            );
            return;
        }
        if let Err(err) = emitter.mirror_evaluated_event(pre, &post) {
            tracing::warn!(
                event_id = %pre.event_id,
                error = %err,
                "governance per-event ER mirror failed; the gap reads as insufficient_evidence \
                 downstream, never compliance"
            );
        }
    }

    /// **#543.** Record + mirror a gate's denial of one evaluated event.
    /// The denial point is the event's whole lifecycle: the inputs and the
    /// typed denial are recorded back-to-back and the ER (a `violation` with
    /// the stable denial codes, or `insufficient_evidence` when the gate
    /// could not determine the outcome) is projected immediately — the round
    /// the call belonged to will typically refuse and never reach the commit
    /// mirror, which is exactly why per-event evidence must not wait for it.
    #[allow(clippy::too_many_arguments)]
    fn governance_denied_event(
        &self,
        session_id: SessionId,
        request_id: &str,
        iteration: u32,
        tool_ordinal: usize,
        call: &ToolCall,
        claims: &VerifiedClaims,
        required: &[Capability],
        public: ErPublicDenialReason,
        internal: &str,
    ) {
        let pre = self.governance_pre_effect(
            session_id,
            request_id,
            iteration,
            tool_ordinal,
            call,
            claims,
            required,
        );
        self.governance_terminal_event(
            &pre,
            EventOutcome::Denied(DeniedOutcome {
                public,
                internal: internal.to_string(),
            }),
        );
    }

    /// **#543.** Record the pre-effect evidence for a turn's memory-write
    /// event and return the record for the later terminal observation. The
    /// pre record must be durably appended **before** the backend write is
    /// invoked (`MemoryControlPlane::record`): a crash mid-write would
    /// otherwise leave a durable memory mutation with no journal record —
    /// exactly the gap #543 exists to close. Callers pair this with
    /// [`Self::governance_terminal_event`] at the terminal observation; on a
    /// control-plane denial (re-verification before the backend) the denial
    /// is recorded via [`Self::governance_memory_denied_event`] instead,
    /// since no backend write is ever attempted.
    fn governance_memory_pre_effect(
        &self,
        session_id: SessionId,
        round_receipt: &ReceiptBody,
        claims: &VerifiedClaims,
        record_digest: &str,
    ) -> Option<PreEffectRecord> {
        let emitter = self.governance.as_ref()?;
        let pre = PreEffectRecord::memory_write(
            &session_id.0.to_string(),
            &round_receipt.receipt_id.to_string(),
            &GrantFacts::from(claims),
            record_digest,
            self.clock.now_ms().get(),
        );
        match emitter.record_pre_effect(&pre) {
            Ok(()) => Some(pre),
            Err(err) => {
                tracing::warn!(
                    event_id = %pre.event_id,
                    error = %err,
                    "governance memory-write pre-effect evidence failed"
                );
                None
            }
        }
    }

    /// **#543.** Record + mirror a memory-write event denied at the control
    /// plane, where no backend write is ever attempted: the denial point is
    /// the event's whole lifecycle, recorded back-to-back.
    fn governance_memory_denied_event(
        &self,
        session_id: SessionId,
        round_receipt: &ReceiptBody,
        claims: &VerifiedClaims,
        record_digest: &str,
        public: ErPublicDenialReason,
        internal: &str,
    ) {
        let pre =
            self.governance_memory_pre_effect(session_id, round_receipt, claims, record_digest);
        self.governance_terminal_event(
            &pre,
            EventOutcome::Denied(DeniedOutcome {
                public,
                internal: internal.to_string(),
            }),
        );
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
    /// [`ToolDef`] per *permitted* tool, projected from its registry schema. An
    /// empty registry — or one whose tools the turn's cap-token all deny —
    /// yields an empty list, so the request is byte-identical to a pre-tool one
    /// and the loop settles on the first provider response.
    ///
    /// The tools to advertise to the provider this turn.
    ///
    /// **gh#415.** Filtered by the turn's cap-token, so the model is told about
    /// exactly the tools it is allowed to call. The invocation-time check in
    /// [`Self::authorize_tool_capabilities`] is unchanged and remains the
    /// enforcement point — this is about what is *disclosed*, not what is
    /// *permitted*.
    ///
    /// Tool names and descriptions are not neutral: `dolthub.execute` or a
    /// customer-named connector tells the model — and anything that can read or
    /// influence the transcript — what this deployment is wired to. A
    /// capability the operator withheld should not be discoverable by reading
    /// the tool list. Advertising unusable tools also spends context on
    /// definitions the turn cannot act on and invites calls certain to be
    /// denied.
    ///
    /// A tool requiring no capability is still checked: the name-scoped
    /// cap-token check and the Cedar `Action::ToolInvoke` decision apply to
    /// every call regardless of declared capabilities, so advertisement must
    /// apply them too or a Cedar-denied tool stays visible.
    fn tool_defs_for(
        &self,
        req: &SubmitRequest,
        provisioning: &PerRequestProvisioning,
        session_id: SessionId,
        now_unix: u64,
    ) -> Vec<ToolDef> {
        self.tools
            .list()
            .into_iter()
            .filter(|t| {
                let name = t.id().0;
                // BOTH gates the invocation path applies, in the same order:
                // the name-scoped token check plus Cedar, then the tool's
                // declared capabilities. Applying only the second would leave a
                // Cedar-denied tool advertised — the same disclosure one gate
                // over.
                self.authorize_tool_invocation(req, provisioning, session_id, now_unix, &name)
                    .is_ok()
                    && self
                        .authorize_tool_capabilities(
                            req,
                            provisioning,
                            now_unix,
                            &name,
                            t.required_capabilities(),
                        )
                        .is_ok()
            })
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
        self.stage_cap_token_for_tool_at(req, provisioning, now_unix, tool, self.cost_units)
    }

    /// The same verification as [`stage_cap_token_for_tool`] at an explicit
    /// cost: control-plane callers verify at the cost the authorizing GATE
    /// used (gh#470 R2), not the chat-turn default.
    fn stage_cap_token_for_tool_at(
        &self,
        req: &SubmitRequest,
        provisioning: &PerRequestProvisioning,
        now_unix: u64,
        tool: &str,
        cost_units: u64,
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
                    cost: cost_units,
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

    /// **ARD-420 / ARD-474.** Check the tool's declared [`Capability`]s against
    /// the cap-token before `invoke` runs. Each required capability (as a
    /// `cap.*` string via [`Capability::as_str`]) is re-verified against the
    /// token (presented as a `tool` fact to the biscuit authorizer) immediately
    /// before the side effect, rather than trusting a cached claims snapshot.
    /// That keeps capability enforcement fail-closed even if future claim
    /// derivation misses a non-standard caveat shape. A missing capability
    /// denies with [`RuntimeError::CapDenied`] before the tool body executes.
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
        for cap in required {
            let label = cap.as_str();
            // ARD-474: re-verify the cap-token for the capability label
            // immediately before invocation.
            if self
                .stage_cap_token_for_tool(req, provisioning, now_unix, &label)
                .is_err()
            {
                return Err(RuntimeError::CapDenied {
                    reason: format!(
                        "tool `{tool_name}` requires capability `{label}` which is not granted by the cap-token"
                    ),
                });
            }
        }
        Ok(())
    }

    /// **ARD-139.** Mint and durably chain a receipt for a session-control
    /// operation that is not a turn: no provider call, no cost-gate
    /// reservation (`cost` is the caller's actual spend — zero for a purely
    /// local operation like an approval propose). Reuses the exact
    /// durability guarantees the turn-receipt commit path gives ordinary
    /// **ARD-139.** Mint a signed receipt for an approval **decision**
    /// (`approval.approve.accepted.v1`/`approval.reject.accepted.v1`) made
    /// against `approval_id`, minted under `cap_token`'s verified subject.
    /// A thin `pub` wrapper over [`commit_control_receipt`](Self::commit_control_receipt)
    /// for a caller that already mutated the approval card through
    /// [`ardur_approvals::ApprovalStore::decide`] elsewhere (the actual
    /// store mutation stays outside this runtime — the same non-turn,
    /// non-cost-gated control-plane receipt pattern
    /// [`checkpoint`](Self)-style operations use elsewhere in this epic).
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the cap-token does not grant `tool` or
    /// the receipt could not be minted.
    pub async fn mint_approval_decision_receipt(
        &self,
        session_id: SessionId,
        cap_token: &CapTokenRef,
        tool: &str,
        verb: &str,
        approval_id: &str,
    ) -> Result<ReceiptId, RuntimeError> {
        let receipt = self
            .commit_control_receipt(
                session_id,
                cap_token,
                tool,
                verb,
                Sha256Digest::of(approval_id.as_bytes()),
                ardur_receipt::CostTuple {
                    tokens_in: 0,
                    tokens_out: 0,
                    cents: 0,
                    wall_ms: 0,
                    attention_score: 0,
                },
                ControlVerifyCost::ApprovalDecision,
            )
            .await?;
        Ok(ReceiptId(receipt.receipt_id))
    }

    /// turns — the same `commit_lock`, the same `chain_tail`, the same
    /// fsync'd receipt log — so a control-plane receipt sits in the *same*
    /// hash chain as ordinary turn receipts.
    #[allow(clippy::too_many_arguments)]
    async fn commit_control_receipt(
        &self,
        session_id: SessionId,
        cap_token: &CapTokenRef,
        tool: &str,
        verb: &str,
        payload_digest: Sha256Digest,
        cost: ardur_receipt::CostTuple,
        verify_cost: ControlVerifyCost,
    ) -> Result<ReceiptBody, RuntimeError> {
        let claims = match verify_cost {
            // gh#470 R2: approval decisions are verified at cost 0,
            // MIRRORING THE COST the HTTP admin gate used, so a
            // legitimately-issued zero-budget token that passed the gate is
            // not priced out of its own receipt. This is cost equality
            // only — the gate's deny list is a fresh empty one and its
            // clock reading is earlier; receipt verification uses the
            // runtime's shared deny list, so a token revoked between gate
            // and receipt still fails here, and receipt minting can still
            // fail for other reasons after the decision persists. Every
            // OTHER control operation keeps the runtime's cost_units:
            // those have no external gate whose cost must be mirrored
            // (review round 2: relaxing them would let a zero-budget token
            // checkpoint, roll back, or accept steers).
            ControlVerifyCost::ApprovalDecision => {
                self.verify_cap_token_for_tool_at(cap_token, tool, 0)?
            }
            ControlVerifyCost::RuntimeDefault => {
                self.verify_cap_token_for_tool_at(cap_token, tool, self.cost_units)?
            }
        };
        let verb = VerbObject::new(verb)
            .map_err(|e| RuntimeError::Internal(anyhow::anyhow!("invalid receipt verb: {e}")))?;
        let now_ms = self.clock.now_ms().get();

        let _commit_guard = self.commit_lock.lock().await;
        let parent_hash = *self.chain_tail.lock();
        let body = ReceiptBody {
            receipt_id: uuid::Uuid::new_v4(),
            parent_hash,
            verb,
            issued_at: ardur_receipt::UnixTsMillis(now_ms),
            subject: ardur_receipt::HolderId(claims.subject.0.clone()),
            cap_token_id: ardur_receipt::TokenId(claims.token_id),
            payload_digest,
            session_id: Some(session_id.0),
            cost,
            tool_calls: Vec::new(),
            provider: None,
        };
        let signed = ReceiptSigner::sign(body, &self.receipt_key)
            .map_err(|e| RuntimeError::Internal(anyhow::anyhow!("receipt mint failed: {e}")))?;
        self.persist_receipt(signed.jws_compact())
            .map_err(|e| RuntimeError::Internal(anyhow::anyhow!("receipt persist failed: {e}")))?;
        *self.chain_tail.lock() = Some(Sha256Digest::of(signed.jws_compact().as_bytes()));
        Ok(signed.body().clone())
    }

    /// **ARD-139.** The propose-half of the approval-gate loop: called after
    /// `authorize_tool_invocation`/`authorize_tool_capabilities` have already
    /// allowed `tool_name`'s call, this additionally requires human sign-off
    /// for any call whose required capabilities intersect
    /// [`approval_gated_capabilities`](Self::approval_gated_capabilities).
    ///
    /// The propose/find half is ONE locked store transaction
    /// ([`ApprovalStore::propose_if_absent`]), so overlapping identical first
    /// calls mint exactly one pending card:
    /// - no card exists: propose one, mint `approval.propose.created.v1`,
    ///   and deny with [`RuntimeError::ApprovalRequired`].
    /// - a `Pending` card exists: deny again with the *same* card id (no
    ///   second receipt — the propose already happened).
    /// - an `Approved` card exists: claim its single execution
    ///   ([`ApprovalStore::claim_execution`], bound to this exact
    ///   tool/arguments/session and stamped with the verified caller). Only
    ///   [`ClaimOutcome::Won`] lets the call proceed; an
    ///   [`ClaimOutcome::AlreadySpent`] observation — an overlapping call won
    ///   the claim first — confers no authority, so this call loops back to
    ///   propose its own pending card instead of riding the spent grant.
    /// - a `Denied` card exists: deny with [`RuntimeError::ApprovalRejected`].
    ///
    /// Returns `Ok(Some(_))` with the won claim's card id when this call owns
    /// the approval's single execution; the caller records the invocation's
    /// outcome on the card once the tool resolves (see
    /// [`Self::record_approval_invocation`]). `Ok(None)` when the gate is not
    /// armed for this call. A tool whose required capabilities do not
    /// intersect the gated set, or a runtime with no
    /// [`approvals`](Self::approvals) store configured, is unaffected — this
    /// stage is a no-op in both cases, matching every other opt-in builder
    /// knob's "absent config behaves as before" contract.
    #[allow(clippy::too_many_arguments)]
    async fn authorize_or_propose_approval(
        &self,
        cap_token: &CapTokenRef,
        session_id: SessionId,
        now_unix: u64,
        tool_name: &str,
        capabilities: &[Capability],
        arguments: &serde_json::Value,
        claimed_by: &str,
    ) -> Result<Option<SpentApproval>, RuntimeError> {
        let Some(store) = &self.approvals else {
            return Ok(None);
        };
        let Some(gated_capability) = capabilities
            .iter()
            .map(Capability::as_str)
            .find(|label| self.approval_gated_capabilities.contains(label.as_str()))
        else {
            return Ok(None);
        };

        let arguments_digest =
            Sha256Digest::of(&serde_json::to_vec(arguments).unwrap_or_default()).to_hex();
        let session_id_str = session_id.0.to_string();

        // Bounded contention loop: an AlreadySpent observation means an
        // overlapping call won the claim between our find and our claim, so
        // loop back to propose this call its own pending card. Every other
        // path exits on the first pass; the bound only guards pathological
        // churn, and exhaustion fails closed rather than proceeding.
        for _ in 0..3 {
            let reason = format!(
                "tool `{tool_name}` requires capability `{gated_capability}`, which is approval-gated"
            );
            let (card, created) = store
                .propose_if_absent(
                    tool_name,
                    gated_capability.as_str(),
                    &arguments_digest,
                    Some(session_id_str.clone()),
                    &reason,
                    now_unix,
                )
                .map_err(|e| {
                    RuntimeError::Internal(anyhow::anyhow!("approval store lookup failed: {e}"))
                })?;
            if created {
                let approval_id = card.id.clone().unwrap_or_default();
                self.commit_control_receipt(
                    session_id,
                    cap_token,
                    &gated_capability,
                    "approval.propose.created.v1",
                    Sha256Digest::of(approval_id.as_bytes()),
                    ardur_receipt::CostTuple {
                        tokens_in: 0,
                        tokens_out: 0,
                        cents: 0,
                        wall_ms: 0,
                        attention_score: 0,
                    },
                    ControlVerifyCost::RuntimeDefault,
                )
                .await?;
                return Err(RuntimeError::ApprovalRequired {
                    approval_id,
                    tool: tool_name.to_string(),
                    reason,
                });
            }
            match card.status {
                // The approval is spent by this call: claim the card so the
                // next identical call proposes a fresh one. Without this, one
                // approval is a standing permission to repeat the call without
                // limit, which is a materially larger grant than the operator
                // gave.
                //
                // Claimed before the tool runs, not after: a card claimed on
                // success only would let a failed-then-retried call reuse the
                // same grant, and a crash between invoke and claim would leave
                // the approval spendable again. Erring toward re-asking the
                // operator is the safe direction for a human-in-the-loop
                // control.
                ApprovalStatus::Approved => {
                    let id = card.id.clone().unwrap_or_default();
                    let binding = ClaimBinding {
                        tool: tool_name,
                        arguments_digest: &arguments_digest,
                        session_id: Some(&session_id_str),
                        claimed_by: Some(claimed_by),
                    };
                    match store.claim_execution(&id, &binding, now_unix) {
                        Ok(ClaimOutcome::Won(_)) => {
                            return Ok(Some(SpentApproval { approval_id: id }));
                        }
                        Ok(ClaimOutcome::AlreadySpent(_)) => {
                            // An overlapping call won this card's single
                            // execution. The observation grants US nothing:
                            // loop to propose our own pending card.
                            continue;
                        }
                        Err(e) => {
                            return Err(RuntimeError::Internal(anyhow::anyhow!(
                                "approval card {id} could not be claimed: {e}"
                            )));
                        }
                        // ClaimOutcome is sealed (non_exhaustive): a future
                        // variant must fail closed, never be read as a win.
                        #[allow(unreachable_patterns)]
                        _ => {
                            return Err(RuntimeError::Internal(anyhow::anyhow!(
                                "approval card {id} returned an unrecognized claim outcome"
                            )));
                        }
                    }
                }
                ApprovalStatus::Denied => {
                    return Err(RuntimeError::ApprovalRejected {
                        approval_id: card.id.unwrap_or_default(),
                        tool: tool_name.to_string(),
                        reason: card.deny_reason.unwrap_or_default(),
                    });
                }
                _ => {
                    return Err(RuntimeError::ApprovalRequired {
                        approval_id: card.id.unwrap_or_default(),
                        tool: tool_name.to_string(),
                        reason: card.reason,
                    });
                }
            }
        }
        Err(RuntimeError::Internal(anyhow::anyhow!(
            "approval claim contention for tool `{tool_name}`"
        )))
    }

    /// Record a won claim's invocation outcome on its card. The card was
    /// consumed before the invocation ran, so this is a post-hoc audit
    /// annotation, never a re-authorization. A recording failure leaves the
    /// card in the explicit ambiguous-effect state (consumed, no outcome) —
    /// logged loudly, never silently remapped.
    fn record_approval_invocation(&self, spent: &Option<SpentApproval>, result: InvocationResult) {
        let (Some(store), Some(spent)) = (&self.approvals, spent) else {
            return;
        };
        let finished_at = self.clock.now_ms().get() / 1000;
        if let Err(e) = store.record_invocation_outcome(
            &spent.approval_id,
            InvocationOutcome {
                result,
                finished_at,
            },
        ) {
            tracing::error!(
                error = %e,
                approval_id = %spent.approval_id,
                "failed to record approval invocation outcome; the card reads as ambiguous-effect"
            );
        }
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
        if self.memory_recall_k == 0 {
            return Ok(request);
        }
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
            .search_scoped(&subject, query, self.memory_recall_k)
            .map_err(|e| RuntimeError::Internal(anyhow::anyhow!("memory recall failed: {e}")))?;
        if hits.is_empty() {
            return Ok(request);
        }

        let mut block = format!(
            "Relevant memories (scoped to verified subject {}):\n",
            claims.subject.0
        );
        let mut injected = 0usize;
        for rec in hits {
            if injected >= self.memory_recall_k {
                break;
            }
            let card = MemoryCard::from_record(&rec);
            let score = memory_recall_score(query, &card);
            if score < self.memory_recall_threshold {
                continue;
            }
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
                "- id={} source={} scope={} confidence={} recall_score={score:.2} receipt={} valid_from={}: {}\n",
                card.record_id,
                source,
                scope,
                confidence,
                receipt,
                card.valid_from.0,
                memory_payload_text(&card.payload)
            ));
            injected += 1;
        }
        if injected == 0 {
            return Ok(request);
        }
        request.messages.insert(0, ChatMessage::system(block));
        Ok(request)
    }

    #[allow(clippy::too_many_arguments)]
    async fn commit_round(
        &self,
        reservation: &mut RuntimeReservation<'_>,
        body: ReceiptBody,
        claims: &VerifiedClaims,
        final_answer: bool,
        iteration: u32,
        req: &SubmitRequest,
        response: &CompletionResponse,
        committed: Option<&Arc<TurnCommitHandshake>>,
        cancel_probe: Option<&CancelProbe>,
    ) -> Result<(SignedReceipt, ReceiptBody), RuntimeError> {
        let signed = {
            let _guard = self.commit_lock.lock().await;
            if cancel_probe.is_some_and(|probe| probe()) {
                reservation.owner.abandon_sync().map_err(settlement_error)?;
                return Err(RuntimeError::TurnCancelled);
            }
            let mut body = body;
            body.parent_hash = *self.chain_tail.lock();
            let signed = ReceiptSigner::sign(body, &self.receipt_key)
                .map_err(|e| RuntimeError::Internal(anyhow::anyhow!("receipt mint failed: {e}")))?;
            let path = self
                .receipt_log
                .as_ref()
                .expect("builder requires receipt storage");
            let expected_log_end = crate::receipts::open_append_no_follow(path)
                .and_then(|f| f.metadata())
                .map_err(|e| RuntimeError::Internal(e.into()))?
                .len();
            if let Some(handshake) = committed {
                if !handshake.begin_persist() {
                    reservation.owner.abandon_sync().map_err(settlement_error)?;
                    return Err(RuntimeError::TurnCancelled);
                }
            }
            reservation
                .owner
                .prepare_receipt(
                    ReceiptCandidate {
                        receipt_id: ReceiptId(signed.body().receipt_id),
                        expected_parent: signed.body().parent_hash,
                        expected_log_end,
                        jws_compact: signed.jws_compact().to_string(),
                        jws_digest: GateSha256::of(signed.jws_compact().as_bytes()),
                    },
                    final_answer,
                    signed.body().cost,
                )
                .map_err(settlement_error)?;
            reservation
                .owner
                .finalize_sync()
                .map_err(settlement_error)?;
            // Any write error is ambiguous: keep exact candidate + actual debit.
            // Never roll back merely because a receipt acknowledgement failed.
            if let Err(e) = crate::receipts::append_at_expected_end(
                path,
                expected_log_end,
                signed.jws_compact(),
            ) {
                self.settlements.stop_admission();
                reservation
                    .owner
                    .receipt_unresolved()
                    .map_err(settlement_error)?;
                return Err(RuntimeError::Internal(anyhow::anyhow!(
                    "settlement receipt unresolved: {e}"
                )));
            }
            *self.chain_tail.lock() = Some(Sha256Digest::of(signed.jws_compact().as_bytes()));
            reservation
                .owner
                .commit_receipt(ReceiptBinding {
                    receipt_id: ReceiptId(signed.body().receipt_id),
                    jws_digest: GateSha256::of(signed.jws_compact().as_bytes()),
                })
                .map_err(settlement_error)?;
            if let Some(handshake) = committed {
                handshake.finish_persist();
            }
            // #502 Seam B7 (Phase 1): mirror the committed round to the
            // opt-in governance emitter, still under the commit lock so a
            // file-backed mirror chains without forking. This is the ONLY
            // mirror point: every abandoned / cancelled path returns before
            // `commit_round`, and the terminal cancellation marker
            // (`record_turn_cancellation`) deliberately does not mirror —
            // a cancelled turn must mint no ER. The native receipt is
            // already durable at this point, so a mirror failure cannot
            // un-commit the round: it is logged (a missing ER reads
            // downstream as `insufficient_evidence`, never compliance) and
            // the turn proceeds.
            if let Some(emitter) = &self.governance {
                let mirrored = signed.body().tool_calls.iter().map(|tc| MirroredToolCall {
                    call_id: tc.call_id.clone(),
                    tool_name: tc.tool_name.clone(),
                    arguments_digest: tc.arguments_digest.to_hex(),
                    output_digest: tc.output_digest.to_hex(),
                    cost: tc.cost,
                });
                let mirrored: Vec<MirroredToolCall> = mirrored.collect();
                let trace_id = signed
                    .body()
                    .session_id
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let step_id = signed.body().receipt_id.to_string();
                let provider_name = signed.body().provider.clone().unwrap_or_default();
                let facts = ErRoundFacts {
                    claims,
                    trace_id: &trace_id,
                    step_id: &step_id,
                    timestamp_millis: signed.body().issued_at.0,
                    tool: &self.tool,
                    provider: &provider_name,
                    tool_calls: &mirrored,
                    // The conservative round-level side-effect fact: the
                    // journal (when configured) appended this round's
                    // transcript entries — actual persistence, not tool count.
                    persisted_transcript: self.journal.is_some(),
                };
                if let Err(err) = emitter.mirror_committed_round(&facts) {
                    tracing::warn!(
                        session_id = %trace_id,
                        receipt_id = %step_id,
                        error = %err,
                        "governance ER mirror failed; native receipt remains the source of truth"
                    );
                }
            }
            signed
        }; // no commit lock across projection, hooks or outward yields
        self.drain_pending_settlements()
            .await
            .map_err(settlement_error)?;
        let receipt = signed.body().clone();
        if let Some(journal) = &self.journal {
            if iteration == 1 {
                if let Some(prompt) = last_user_message(&req.messages) {
                    journal
                        .append(JournalEntry::UserMessage {
                            content: prompt.into(),
                            at: receipt.issued_at,
                        })
                        .await
                        .map_err(|e| {
                            RuntimeError::Internal(anyhow::anyhow!(
                                "settlement user journal failed: {e}"
                            ))
                        })?;
                }
            }
            journal
                .append(JournalEntry::AssistantMessage {
                    content: response.content.clone(),
                    at: receipt.issued_at,
                    receipt_id: ReceiptId(receipt.receipt_id),
                })
                .await
                .map_err(|e| {
                    RuntimeError::Internal(anyhow::anyhow!(
                        "settlement assistant journal failed: {e}"
                    ))
                })?;
        }
        Ok((signed, receipt))
    }

    /// Append a signed receipt's compact JWS to the durable receipt log
    /// (one line, fsynced), if a log path is configured.
    fn persist_receipt(&self, jws_compact: &str) -> std::io::Result<()> {
        let Some(path) = &self.receipt_log else {
            return Ok(());
        };
        use std::io::Write as _;
        let mut file = crate::receipts::open_append_no_follow(path)?;
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
    /// - [`ReconciliationError::Undecidable`] while a live turn still owns its
    ///   receipt/journal projection, or if
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

        // A committed receipt can precede its still-live journal projection.
        // Refuse rather than wait on a running turn, and prevent new admission
        // through the sweep. Final observational hooks do not hold this permit.
        let _idle_projection =
            self.economic_admission
                .try_lock()
                .map_err(|_| ReconciliationError::Undecidable {
                    reason: "receipt reconciliation requires idle turn projection".into(),
                })?;
        // Also serialize against internal control receipts and concurrent sweeps.
        let _commit = self.commit_lock.lock().await;
        let chain = load_persisted_chain(receipt_log)?;
        let receipt_jwks = ardur_receipt::Jwks::from_public_key(&self.receipt_key.public_key());
        verify_persisted_chain_with_jwks(&chain, &receipt_jwks)?;
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
                JournalEntry::AssistantMessage { receipt_id, .. }
                | JournalEntry::ToolInvocation { receipt_id, .. } => Some(receipt_id.0),
                _ => None,
            })
            .collect();

        let snapshots = self
            .settlement_supervisor()
            .durable_snapshots()
            .map_err(|e| ReconciliationError::Undecidable {
                reason: e.to_string(),
            })?;
        let (modern_receipts, recovery) =
            crate::reconcile::owned_recovery_entries(&chain, &snapshots, session_id, &entries)?;
        let relevant_receipt_count = chain
            .iter()
            .filter(|receipt| {
                recovery.contains_key(&receipt.body.receipt_id)
                    || (receipt.body.session_id == Some(session_id.0)
                        && !modern_receipts.contains(&receipt.body.receipt_id))
            })
            .count();
        let orphan_indices: Vec<usize> = chain
            .iter()
            .enumerate()
            .filter(|(_, receipt)| {
                if let Some(entries) = recovery.get(&receipt.body.receipt_id) {
                    // Modern evidence is never authority to truncate the receipt chain.
                    return self.reconciliation_strategy != ReconciliationStrategy::TruncateOrphans
                        && !entries.is_empty();
                }
                receipt.body.session_id == Some(session_id.0)
                && !journaled.contains(&receipt.body.receipt_id)
                && !modern_receipts.contains(&receipt.body.receipt_id)
                // Genuine legacy completions only; a control receipt is not an answer.
                && receipt.body.verb == self.verb
                && !matches!(receipt.body.verb.as_str(),
                    CANCELLED_VERB | "approval.propose.created.v1"
                    | "approval.approve.accepted.v1" | "approval.reject.accepted.v1"
                    | "tool.grant.allow.v1")
            })
            .map(|(i, _)| i)
            .collect();
        let orphan_receipt_ids: Vec<uuid::Uuid> = orphan_indices
            .iter()
            .map(|&i| chain[i].body.receipt_id)
            .collect();

        let mut report = ReconciliationReport {
            receipt_count: relevant_receipt_count,
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
                let now = self.clock.now_ms().get();
                let mut count = 0;
                for &i in &orphan_indices {
                    let rid = chain[i].body.receipt_id;
                    if let Some(entries) = recovery.get(&rid) {
                        for entry in entries {
                            journal.append(entry.clone()).await?;
                            count += 1;
                        }
                        continue;
                    }
                    journal
                        .append(JournalEntry::AssistantMessage {
                            content: format!(
                                "[reconciled] recovered orphan receipt {rid}: its receipt was \
                                 durably minted (stage 6) but the process crashed before the \
                                 journal append (stage 10), so the original assistant content is \
                                 unrecoverable."
                            ),
                            at: ardur_cost_gate::UnixTsMillis(now),
                            receipt_id: ReceiptId(rid),
                        })
                        .await?;
                    count += 1;
                }
                report.action = ReconciliationAction::AppendedSyntheticJournal { count };
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
        replace_receipt_log_no_follow(receipt_log, body.as_bytes())
            .map_err(ReconciliationError::Io)?;

        // Reset the in-memory chain tail to the new last receipt (or None if the
        // whole chain was orphaned), so the next turn chains correctly. build()
        // had seeded it from the now-removed orphan tail.
        *self.chain_tail.lock() = retained
            .last()
            .map(|r| Sha256Digest::of(r.jws_compact.as_bytes()));
        Ok(())
    }
}

/// Atomic #359 handshake between the HTTP caller and receipt persist.
///
/// `request_cancel` only succeeds against Live. `begin_persist` only succeeds
/// against Live or Committed. A grace timeout concurrent with `persist_receipt`'s
/// write+fsync therefore sees Persisting and waits, instead of 504-while-billing.
pub struct TurnCommitHandshake {
    phase: AtomicU8,
}

const HS_LIVE: u8 = 0;
const HS_CANCELLED: u8 = 1;
const HS_PERSISTING: u8 = 2;
const HS_COMMITTED: u8 = 3;

impl TurnCommitHandshake {
    /// A live handshake: not cancelled, no persist in flight.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            phase: AtomicU8::new(HS_LIVE),
        })
    }

    /// Mark the caller gone, but only if persist has not already begun.
    pub fn request_cancel(&self) {
        let _ =
            self.phase
                .compare_exchange(HS_LIVE, HS_CANCELLED, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// True only when cancel won against Live (no persist has begun).
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.phase.load(Ordering::SeqCst) == HS_CANCELLED
    }

    /// Persist write+fsync is in flight, or at least one receipt is durable.
    /// HTTP must not 504 in either case.
    #[must_use]
    pub fn must_wait_for_outcome(&self) -> bool {
        matches!(
            self.phase.load(Ordering::SeqCst),
            HS_PERSISTING | HS_COMMITTED
        )
    }

    /// At least one receipt has finished persist (not merely in flight).
    #[must_use]
    pub fn ever_committed(&self) -> bool {
        self.phase.load(Ordering::SeqCst) == HS_COMMITTED
    }

    /// Claim the right to run `persist_receipt`. False if cancel already won.
    pub fn begin_persist(&self) -> bool {
        loop {
            match self.phase.load(Ordering::SeqCst) {
                HS_LIVE => {
                    if self
                        .phase
                        .compare_exchange(
                            HS_LIVE,
                            HS_PERSISTING,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_ok()
                    {
                        return true;
                    }
                }
                HS_COMMITTED => {
                    if self
                        .phase
                        .compare_exchange(
                            HS_COMMITTED,
                            HS_PERSISTING,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_ok()
                    {
                        return true;
                    }
                }
                HS_CANCELLED => return false,
                HS_PERSISTING => return true,
                _ => return false,
            }
        }
    }

    /// Receipt write+fsync succeeded.
    pub fn finish_persist(&self) {
        self.phase.store(HS_COMMITTED, Ordering::SeqCst);
    }

    /// Receipt write failed; revert to Live (or Committed if a prior round billed).
    pub fn abort_persist(&self, previously_committed: bool) {
        self.phase.store(
            if previously_committed {
                HS_COMMITTED
            } else {
                HS_LIVE
            },
            Ordering::SeqCst,
        );
    }
}

/// A synchronous probe reporting whether the turn's caller has gone away.
/// Consulted at the commit gate (after each provider round and again after
/// tool execution, immediately before the receipt/journal/billing commit): a
/// probe reporting `true` aborts the turn with [`RuntimeError::TurnCancelled`]
/// and no *further* side effects.
///
/// The HTTP server builds this over a per-turn `AtomicBool` that
/// `CallerGoneOnDrop` (hang-up) or `submit_chat`'s timeout arm (deadline)
/// stores `true` into **synchronously in the signalling thread**. That is the
/// starvation-safety argument: the flag does not wait to be scheduled the way
/// `oneshot::Sender::is_closed()` does. Do not substitute `is_closed()` here
/// (#359).
pub type CancelProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// gh#452 settlement-record reasons — the refusal class each post-provider
/// refusal settles its KNOWN incurred usage under. Distinct, stable strings so
/// settlement aggregation can classify without parsing error text.
const UNKNOWN_TOOL_SETTLEMENT: &str = "refusal:unknown_tool";
const TOOL_CAPACITY_SETTLEMENT: &str = "refusal:capacity";
const TOOL_AUTH_SETTLEMENT: &str = "refusal:tool_authorization";
const CAPABILITY_SETTLEMENT: &str = "refusal:capability_denied";
const APPROVAL_SETTLEMENT: &str = "refusal:approval_required";
const APPROVAL_REJECTED_SETTLEMENT: &str = "refusal:approval_rejected";
const TOOL_ERROR_SETTLEMENT: &str = "refusal:tool_error";
const TOOL_TIMEOUT_SETTLEMENT: &str = "refusal:tool_timeout_uncertain_effect";
const SCAN_SETTLEMENT: &str = "refusal:output_scan";

#[async_trait::async_trait]
impl ChatRuntime for FusedRuntime {
    async fn submit(&self, req: SubmitRequest) -> Result<SubmitResult, RuntimeError> {
        self.submit_inner(req, PerRequestProvisioning::default(), None, None)
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
        self.submit_inner(req, provisioning, None, None).await
    }

    /// Submit a turn with a [`CancelProbe`]: the probe is consulted after every
    /// provider round and again after tool execution, immediately before the
    /// receipt/journal/billing commit. A gone caller aborts with
    /// [`RuntimeError::TurnCancelled`] so the current round does not mint
    /// (#359). Already-committed earlier tool-loop rounds cannot be un-minted
    /// (the receipt log is append-only); that residual is tracked separately.
    pub async fn submit_with_cancellation(
        &self,
        req: SubmitRequest,
        provisioning: PerRequestProvisioning,
        cancel_probe: CancelProbe,
        committed: Option<Arc<TurnCommitHandshake>>,
    ) -> Result<SubmitResult, RuntimeError> {
        self.submit_inner(req, provisioning, Some(cancel_probe), committed)
            .await
    }

    /// #359 commit gate. If the caller is gone, record the operator's known
    /// expense (gh#452 — the provider round may already have run), release the
    /// reservation (the `ReservationCancelGuard` then finds nothing to refund —
    /// `take_reservation` is None after an explicit release) and abort with
    /// TurnCancelled.
    async fn abort_if_caller_gone<'a>(
        &self,
        session_id: SessionId,
        reservation: RuntimeReservation<'a>,
        _known_incurred: RuntimeCostTuple,
        cancel_probe: &Option<CancelProbe>,
    ) -> Result<RuntimeReservation<'a>, RuntimeError> {
        if cancel_probe.as_ref().is_some_and(|probe| probe()) {
            self.release(reservation).await;
            let err = RuntimeError::TurnCancelled;
            self.fire_error(session_id, LifecyclePhase::Provider, &err)
                .await;
            return Err(err);
        }
        Ok(reservation)
    }

    /// Settle a turn abandoned mid tool-loop (#422).
    ///
    /// The receipt log is append-only, so a cancel arriving during iteration N
    /// cannot un-mint the receipts iterations 1..N-1 already persisted. Two
    /// things must still hold:
    ///
    /// 1. The turn must not be reported as succeeding. Returning an earlier
    ///    round's receipt as the outcome would hand the caller a "result" that
    ///    is really a mid-loop tool round — in practice an empty assistant
    ///    message — and mark a turn complete that never produced an answer.
    /// 2. The chain must not end on an intermediate round, which would imply a
    ///    settled turn. When at least one round committed, a terminal
    ///    `llm.completion.cancelled.v1` receipt is appended so the chain records
    ///    that the turn ended without settling.
    ///
    /// A turn cancelled before anything committed appends nothing — that is
    /// #359's contract and it is preserved exactly.
    ///
    /// The cancellation receipt is chained and signed like any other, so
    /// `verify_persisted_chain` still passes. Minting it is best-effort: a
    /// failure here is logged, never converted into a different error, because
    /// the turn is already being abandoned.
    async fn record_turn_cancellation(
        &self,
        session_id: SessionId,
        claims: &VerifiedClaims,
        committed_rounds: u32,
    ) {
        if committed_rounds == 0 {
            return;
        }
        let verb = match VerbObject::new(CANCELLED_VERB) {
            Ok(verb) => verb,
            Err(e) => {
                tracing::error!(error = %e, "invalid cancellation verb");
                return;
            }
        };
        let now_ms = self.clock.now_ms().get();

        let _commit_guard = self.commit_lock.lock().await;
        let parent_hash = *self.chain_tail.lock();
        let body = ReceiptBody {
            receipt_id: uuid::Uuid::new_v4(),
            parent_hash,
            verb,
            issued_at: ardur_receipt::UnixTsMillis(now_ms),
            subject: ardur_receipt::HolderId(claims.subject.0.clone()),
            cap_token_id: ardur_receipt::TokenId(claims.token_id),
            // The payload is the abandonment itself, not model output.
            payload_digest: Sha256Digest::of(b"turn cancelled before settling"),
            session_id: Some(session_id.0),
            // ZERO, deliberately. This is a marker, not a billed event: the
            // cost of every committed round is already carried by that round's
            // own receipt. Chain aggregators sum `cost` across all receipts
            // without inspecting the verb (`AppState::receipt_stats`,
            // `ardur_admin_ui::costs::aggregate_receipts`), so restating the
            // cumulative total here would double-count a cancelled turn — a
            // single 5-cent round would report as 10 cents. The cost gate
            // finalized each round exactly once and this record must not
            // change that arithmetic.
            cost: ardur_receipt::CostTuple {
                tokens_in: 0,
                tokens_out: 0,
                cents: 0,
                wall_ms: 0,
                attention_score: 0,
            },
            tool_calls: Vec::new(),
            provider: Some(self.provider.name()),
        };
        let signed = match ReceiptSigner::sign(body, &self.receipt_key) {
            Ok(signed) => signed,
            Err(e) => {
                tracing::error!(error = %e, "cancellation receipt mint failed");
                return;
            }
        };
        if let Err(e) = self.persist_receipt(signed.jws_compact()) {
            tracing::error!(error = %e, "cancellation receipt persist failed");
            return;
        }
        *self.chain_tail.lock() = Some(Sha256Digest::of(signed.jws_compact().as_bytes()));
        tracing::info!(
            rounds = committed_rounds,
            "turn cancelled mid tool-loop; terminal cancellation receipt recorded"
        );
    }

    async fn submit_inner(
        &self,
        req: SubmitRequest,
        provisioning: PerRequestProvisioning,
        cancel_probe: Option<CancelProbe>,
        committed: Option<Arc<TurnCommitHandshake>>,
    ) -> Result<SubmitResult, RuntimeError> {
        let session_id = req.session_id;
        let turn_start_ms = self.clock.now_ms().get();
        let turn_start_unix = turn_start_ms / 1000;

        // ---- 1. cap-token: parse + verify against the root, audience, tool,
        //         and deny-list.
        let claims = match self.stage_cap_token(&req, &provisioning, turn_start_unix) {
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
        if cancel_probe.as_ref().is_some_and(|probe| probe()) {
            return Err(RuntimeError::TurnCancelled);
        }
        // Waiting callers own no settlement capacity or budget. Declare the
        // permit before the owner so cancellation drops the owner first.
        let mut economic_permit = Some(self.economic_admission.lock().await);
        if cancel_probe.as_ref().is_some_and(|probe| probe()) {
            return Err(RuntimeError::TurnCancelled);
        }
        let mut owner = self.begin_settlement(session_id, &claims, &provisioning)?;
        let gate_token_id = match self.stage_cost_setup(&claims, &provisioning).await {
            Ok(gate_token_id) => gate_token_id,
            Err(err) => {
                self.fire_error(session_id, LifecyclePhase::Submit, &err)
                    .await;
                return Err(err);
            }
        };

        // The tools advertised to the provider every iteration of this turn.
        // gh#415: narrowed to what this turn's cap-token permits, so the model
        // is not told about capabilities the operator withheld.
        let tool_defs_now_unix = self.clock.now_ms().get() / 1000;
        let tool_defs = self.tool_defs_for(&req, &provisioning, session_id, tool_defs_now_unix);

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
        // #422: how many tool-loop rounds have durably committed a receipt.
        // A cancel after >= 1 round cannot un-mint those receipts, so the chain
        // gets a terminal cancellation record instead.
        let mut committed_rounds: u32 = 0;

        let (receipt, final_content) = loop {
            // There is no current-round hold or expense yet. In particular a
            // cancelled later round must not become an admission refusal just
            // because earlier committed rounds depleted the caller's budget.
            if cancel_probe.as_ref().is_some_and(|probe| probe()) {
                self.record_turn_cancellation(session_id, &claims, committed_rounds)
                    .await;
                let err = RuntimeError::TurnCancelled;
                self.fire_error(session_id, LifecyclePhase::Provider, &err)
                    .await;
                return Err(err);
            }
            iteration += 1;
            // ARD-480: expiry-sensitive re-verification happens throughout the
            // tool loop, so use the clock at this iteration, not turn start.
            let iteration_now_ms = self.clock.now_ms().get();

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
                    // No current hold exists here. Preserve first-turn setup
                    // refusals, but terminalize an abandoned committed loop.
                    let err = if committed_rounds > 0
                        && cancel_probe.as_ref().is_some_and(|probe| probe())
                    {
                        self.record_turn_cancellation(session_id, &claims, committed_rounds)
                            .await;
                        RuntimeError::TurnCancelled
                    } else {
                        err
                    };
                    self.fire_error(session_id, LifecyclePhase::Submit, &err)
                        .await;
                    return Err(err);
                }
            };

            // 3'. cost-gate admit (per iteration).
            let request_digest = GateSha256::of(
                &serde_json::to_vec(&iter_request).map_err(|e| RuntimeError::Internal(e.into()))?,
            );
            let mut reservation = match self
                .admit_round(
                    &mut owner,
                    AdmissionRequest {
                        cap_token_id: gate_token_id,
                        projected_envelope: self.envelope,
                        provider_id: self.gate_provider_id.clone(),
                        model_id: self.gate_model_id.clone(),
                        request_digest,
                    },
                    iter_request.request_id.0,
                )
                .await
            {
                Ok(reservation) => reservation,
                Err(e) => {
                    // Failed admission returned no reservation to release.
                    let err = if committed_rounds > 0
                        && cancel_probe.as_ref().is_some_and(|probe| probe())
                    {
                        self.record_turn_cancellation(session_id, &claims, committed_rounds)
                            .await;
                        RuntimeError::TurnCancelled
                    } else {
                        e
                    };
                    self.fire_error(session_id, LifecyclePhase::Submit, &err)
                        .await;
                    return Err(err);
                }
            };

            // A queued cancellation must not start a new provider round. This
            // also covers abandonment during outbound scanning or admission.
            reservation = match self
                .abort_if_caller_gone(
                    session_id,
                    reservation,
                    RuntimeCostTuple::ZERO,
                    &cancel_probe,
                )
                .await
            {
                Ok(reservation) => reservation,
                Err(RuntimeError::TurnCancelled) => {
                    self.record_turn_cancellation(session_id, &claims, committed_rounds)
                        .await;
                    return Err(RuntimeError::TurnCancelled);
                }
                Err(err) => return Err(err),
            };

            // 5. provider dispatch. #543: keep the round's request id — the
            // tool-event identity discriminator — past the request's move.
            let iter_request_id = iter_request.request_id.0.to_string();
            reservation
                .owner
                .observe_provider(ProviderEvidence::DispatchIntent)
                .map_err(settlement_error)?;
            let response = self.provider.complete(iter_request).await;
            if let Ok(response) = &response {
                reservation
                    .owner
                    .observe_provider(ProviderEvidence::Observed {
                        usage: Some(UsageSnapshot {
                            input_tokens: u64::from(response.usage.tokens_in),
                            output_tokens: u64::from(response.usage.tokens_out),
                        }),
                        cost: response.cost,
                        provenance: CostProvenance::ResponseCost,
                        finished: true,
                        interrupted: false,
                    })
                    .map_err(settlement_error)?;
            }
            // ARD-501: a slow completion can outlive the reservation TTL. Refresh
            // the lease now the provider has returned so the finalize below (and
            // the post-receipt hooks between here and it) does not discard a turn
            // the caller has already received. No-op once finalized.
            if response.is_ok() {
                self.gate.touch_reservation(reservation.reservation_id);
            }

            // A failed provider await can also abandon the caller. ProviderError
            // has no usage, so only a returned response supplies known expense.
            let known = response.as_ref().map_or(RuntimeCostTuple::ZERO, |r| r.cost);
            reservation = match self
                .abort_if_caller_gone(session_id, reservation, known, &cancel_probe)
                .await
            {
                Ok(reservation) => reservation,
                Err(RuntimeError::TurnCancelled) => {
                    self.record_turn_cancellation(session_id, &claims, committed_rounds)
                        .await;
                    return Err(RuntimeError::TurnCancelled);
                }
                Err(err) => return Err(err),
            };

            let response = match response {
                Ok(response) => response,
                Err(provider_err) => {
                    self.release_failure(reservation).await;
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
            // Refuse the entire batch before any effect or bounded observation.
            if requested.len() > crate::settlement::LIMITS.max_tools_per_round {
                let err = RuntimeError::Internal(anyhow::anyhow!(
                    "tool batch exceeds settlement capacity: {} > {}",
                    requested.len(),
                    crate::settlement::LIMITS.max_tools_per_round
                ));
                let settlement = self
                    .settle_refusal(
                        session_id,
                        reservation,
                        response.cost,
                        TOOL_CAPACITY_SETTLEMENT,
                    )
                    .await;
                self.fire_error(session_id, LifecyclePhase::Provider, &err)
                    .await;
                return match settlement {
                    Ok(()) => Err(err),
                    Err(error) => Err(error),
                };
            }
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
                for (tool_ordinal, call) in requested.iter().enumerate() {
                    // #359: do not authorize or invoke further tools once the
                    // caller is gone — later tools would otherwise run with no
                    // receipt attesting their effects.
                    let known_incurred = response.cost.saturating_add(&tool_cost);
                    reservation = match self
                        .abort_if_caller_gone(
                            session_id,
                            reservation,
                            known_incurred,
                            &cancel_probe,
                        )
                        .await
                    {
                        Ok(reservation) => reservation,
                        Err(RuntimeError::TurnCancelled) => {
                            self.record_turn_cancellation(session_id, &claims, committed_rounds)
                                .await;
                            return Err(RuntimeError::TurnCancelled);
                        }
                        Err(err) => return Err(err),
                    };
                    let Some(tool) = self.tools.get(&ToolId::new(&call.name)) else {
                        let err = RuntimeError::UnknownTool {
                            tool: call.name.clone(),
                        };
                        // #543: the requested call was evaluated and rejected
                        // (no registered tool) — record the denial before the
                        // refusal settles, since this round never commits.
                        self.governance_denied_event(
                            session_id,
                            &iter_request_id,
                            iteration,
                            tool_ordinal,
                            call,
                            &claims,
                            &[],
                            ErPublicDenialReason::PolicyDenied,
                            "unknown_tool",
                        );
                        let known = response.cost.saturating_add(&tool_cost);
                        let settlement = self
                            .settle_refusal(session_id, reservation, known, UNKNOWN_TOOL_SETTLEMENT)
                            .await;
                        self.fire_error(session_id, LifecyclePhase::Provider, &err)
                            .await;
                        return match settlement {
                            Ok(()) => Err(err),
                            Err(settle_err) => Err(settle_err),
                        };
                    };
                    let tool_auth_now_unix = self.clock.now_ms().get() / 1000;
                    if let Err(err) = self.authorize_tool_invocation(
                        &req,
                        &provisioning,
                        session_id,
                        tool_auth_now_unix,
                        &call.name,
                    ) {
                        // #543: cap-token/Cedar tool-invoke denial.
                        let (public, internal) = tool_auth_denial_classification(&err);
                        self.governance_denied_event(
                            session_id,
                            &iter_request_id,
                            iteration,
                            tool_ordinal,
                            call,
                            &claims,
                            tool.required_capabilities(),
                            public,
                            internal,
                        );
                        let known = response.cost.saturating_add(&tool_cost);
                        let settlement = self
                            .settle_refusal(session_id, reservation, known, TOOL_AUTH_SETTLEMENT)
                            .await;
                        self.fire_error(session_id, LifecyclePhase::Submit, &err)
                            .await;
                        return match settlement {
                            Ok(()) => Err(err),
                            Err(settle_err) => Err(settle_err),
                        };
                    }
                    // ARD-420: enforce required_capabilities() against the
                    // cap-token's tool allowlist before the tool body runs.
                    if let Err(err) = self.authorize_tool_capabilities(
                        &req,
                        &provisioning,
                        tool_auth_now_unix,
                        &call.name,
                        tool.required_capabilities(),
                    ) {
                        // #543: declared-capability denial.
                        self.governance_denied_event(
                            session_id,
                            &iter_request_id,
                            iteration,
                            tool_ordinal,
                            call,
                            &claims,
                            tool.required_capabilities(),
                            ErPublicDenialReason::PolicyDenied,
                            "capability_not_granted",
                        );
                        let known = response.cost.saturating_add(&tool_cost);
                        let settlement = self
                            .settle_refusal(session_id, reservation, known, CAPABILITY_SETTLEMENT)
                            .await;
                        self.fire_error(session_id, LifecyclePhase::Submit, &err)
                            .await;
                        return match settlement {
                            Ok(()) => Err(err),
                            Err(settle_err) => Err(settle_err),
                        };
                    }
                    // ARD-139: a call whose required capabilities include an
                    // approval-gated one needs human sign-off even though the
                    // cap-token/cedar checks above already allow it. gh#497:
                    // only a WON claim (bound to this exact call and stamped
                    // with the verified caller) lets the invocation proceed.
                    let approval_result = self
                        .authorize_or_propose_approval(
                            &req.cap_token,
                            session_id,
                            tool_auth_now_unix,
                            &call.name,
                            tool.required_capabilities(),
                            &call.arguments,
                            &claims.subject.0,
                        )
                        .await;
                    // #543: an already-evaluated approval denial is evidence-
                    // recorded BEFORE the cancellation gate can return — a
                    // caller disconnecting during the approval wait must not
                    // strand the evaluated event.
                    if let Err(err) = &approval_result {
                        let (public, internal) = approval_denial_classification(err);
                        self.governance_denied_event(
                            session_id,
                            &iter_request_id,
                            iteration,
                            tool_ordinal,
                            call,
                            &claims,
                            tool.required_capabilities(),
                            public,
                            internal,
                        );
                    }
                    reservation = match self
                        .abort_if_caller_gone(
                            session_id,
                            reservation,
                            response.cost.saturating_add(&tool_cost),
                            &cancel_probe,
                        )
                        .await
                    {
                        Ok(reservation) => reservation,
                        Err(RuntimeError::TurnCancelled) => {
                            self.record_turn_cancellation(session_id, &claims, committed_rounds)
                                .await;
                            return Err(RuntimeError::TurnCancelled);
                        }
                        Err(err) => return Err(err),
                    };
                    let spent_approval = match approval_result {
                        Ok(spent) => spent,
                        Err(err) => {
                            let known = response.cost.saturating_add(&tool_cost);
                            let reason = if matches!(err, RuntimeError::ApprovalRequired { .. }) {
                                APPROVAL_SETTLEMENT
                            } else {
                                APPROVAL_REJECTED_SETTLEMENT
                            };
                            let settlement = self
                                .settle_refusal(session_id, reservation, known, reason)
                                .await;
                            self.fire_error(session_id, LifecyclePhase::Submit, &err)
                                .await;
                            return match settlement {
                                Ok(()) => Err(err),
                                Err(settle_err) => Err(settle_err),
                            };
                        }
                    };
                    let ctx = self.tool_context(&req.cap_token, session_id);
                    // #543: every admission gate passed. Observe the
                    // dispatch intent first (it is fallible), then
                    // persist the pre AFTER it succeeds and BEFORE `invoke`:
                    // an observation failure then leaves no stranded pre the
                    // sweep would sign as `effect_unobserved` for a tool that
                    // never dispatched, and the pre is still durable before
                    // the effect.
                    self.observe_tool(
                        &mut reservation,
                        tool_ordinal,
                        call,
                        ToolEffect::DispatchIntent,
                        OutputAdmission::NotScanned,
                    )?;
                    let governance_pre = self.governance_pre_effect(
                        session_id,
                        &iter_request_id,
                        iteration,
                        tool_ordinal,
                        call,
                        &claims,
                        tool.required_capabilities(),
                    );
                    let tool_result = tokio::time::timeout(
                        self.tool_timeout,
                        tool.invoke(&ctx, call.arguments.clone()),
                    )
                    .await;
                    // Preserve the verified invocation outcome even if caller
                    // cancellation wins. A timeout has no verified outcome.
                    let mut known = response.cost.saturating_add(&tool_cost);
                    let scan_result = match &tool_result {
                        Ok(Ok(output)) => {
                            // #543: scan first, then the terminal evidence,
                            // then the fallible settlement observation — an
                            // already-observed result is never stranded behind
                            // a settlement persistence failure or a later
                            // cancel gate. A scanner operational failure (a
                            // filter error, not a block verdict) records
                            // admission as undetermined →
                            // insufficient_evidence, never a guessed violation.
                            let scan_result =
                                self.scan_tool_output(&call.name, &output.content).await;
                            self.governance_terminal_event(
                                &governance_pre,
                                EventOutcome::Completed(CompletedOutcome {
                                    output_digest: Sha256Digest::of(
                                        &serde_json::to_vec(&output.content).unwrap_or_default(),
                                    )
                                    .to_hex(),
                                    cost: output.cost,
                                    output_admission: match &scan_result {
                                        Ok(()) => EvidenceOutputAdmission::Allowed,
                                        Err(RuntimeError::InjectionBlocked { .. }) => {
                                            EvidenceOutputAdmission::Blocked
                                        }
                                        Err(_) => EvidenceOutputAdmission::Undetermined,
                                    },
                                }),
                            );
                            self.observe_tool(
                                &mut reservation,
                                tool_ordinal,
                                call,
                                ToolEffect::Completed {
                                    output_digest: GateSha256::of(
                                        &serde_json::to_vec(&output.content)
                                            .map_err(|e| RuntimeError::Internal(e.into()))?,
                                    ),
                                    cost: output.cost,
                                },
                                if scan_result.is_ok() {
                                    OutputAdmission::Allowed
                                } else {
                                    OutputAdmission::Blocked
                                },
                            )?;
                            self.record_approval_invocation(
                                &spent_approval,
                                InvocationResult::Completed,
                            );
                            known = known.saturating_add(&output.cost);
                            Some(scan_result)
                        }
                        Ok(Err(tool_err)) => {
                            // #543: terminal evidence BEFORE the fallible
                            // settlement observation, so neither a persist
                            // failure nor a cancel strands the observation —
                            // and typed pre-effect refusals record as
                            // canonical denials, not unknown effects.
                            self.governance_terminal_event(
                                &governance_pre,
                                Self::tool_error_event_outcome(tool_err),
                            );
                            self.observe_tool(
                                &mut reservation,
                                tool_ordinal,
                                call,
                                ToolEffect::Failed {
                                    class: ToolFailureClass::Execution,
                                    effect_unknown: true,
                                },
                                OutputAdmission::NotScanned,
                            )?;
                            self.record_approval_invocation(
                                &spent_approval,
                                InvocationResult::Failed,
                            );
                            None
                        }
                        Err(_) => {
                            // #543: timeout with a possible effect — terminal
                            // evidence before the fallible observation.
                            self.governance_terminal_event(
                                &governance_pre,
                                EventOutcome::TimeoutUnknown,
                            );
                            self.observe_tool(
                                &mut reservation,
                                tool_ordinal,
                                call,
                                ToolEffect::InterruptedUnknown,
                                OutputAdmission::NotScanned,
                            )?;
                            None
                        }
                    };
                    // Failed awaits and timeouts obey the same cancellation
                    // economics as successful returns, before any error hook.
                    reservation = match self
                        .abort_if_caller_gone(session_id, reservation, known, &cancel_probe)
                        .await
                    {
                        Ok(reservation) => reservation,
                        Err(RuntimeError::TurnCancelled) => {
                            self.record_turn_cancellation(session_id, &claims, committed_rounds)
                                .await;
                            return Err(RuntimeError::TurnCancelled);
                        }
                        Err(err) => return Err(err),
                    };
                    let output = match tool_result {
                        Ok(Ok(output)) => output,
                        Ok(Err(tool_err)) => {
                            let err = map_tool_error(tool_err, &call.name);
                            let known = response.cost.saturating_add(&tool_cost);
                            let settlement = self
                                .settle_refusal(
                                    session_id,
                                    reservation,
                                    known,
                                    TOOL_ERROR_SETTLEMENT,
                                )
                                .await;
                            self.fire_error(session_id, LifecyclePhase::Provider, &err)
                                .await;
                            return match settlement {
                                Ok(()) => Err(err),
                                Err(settle_err) => Err(settle_err),
                            };
                        }
                        Err(_elapsed) => {
                            // Timed out with the invocation's effect unknown:
                            // the spent card is deliberately left WITHOUT an
                            // outcome record — the explicit ambiguous-effect
                            // state. The provider round is KNOWN and debited;
                            // the tool's cost is not (gh#452).
                            let known = response.cost.saturating_add(&tool_cost);
                            let settlement = self
                                .settle_refusal(
                                    session_id,
                                    reservation,
                                    known,
                                    TOOL_TIMEOUT_SETTLEMENT,
                                )
                                .await;
                            let err = RuntimeError::ToolTimeout {
                                tool: call.name.clone(),
                            };
                            self.fire_error(session_id, LifecyclePhase::Provider, &err)
                                .await;
                            return match settlement {
                                Ok(()) => Err(err),
                                Err(settle_err) => Err(settle_err),
                            };
                        }
                    };

                    // The scan decision and its terminal evidence were made
                    // in the success arm above (before the first fallible
                    // settlement observation); this path only handles the
                    // admission outcome.
                    let scan_result =
                        scan_result.expect("the success arm produced the scan decision");
                    reservation = match self
                        .abort_if_caller_gone(session_id, reservation, known, &cancel_probe)
                        .await
                    {
                        Ok(reservation) => reservation,
                        Err(RuntimeError::TurnCancelled) => {
                            self.record_turn_cancellation(session_id, &claims, committed_rounds)
                                .await;
                            return Err(RuntimeError::TurnCancelled);
                        }
                        Err(err) => return Err(err),
                    };
                    if let Err(err) = scan_result {
                        let known = response
                            .cost
                            .saturating_add(&tool_cost)
                            .saturating_add(&output.cost);
                        let settlement = self
                            .settle_refusal(session_id, reservation, known, SCAN_SETTLEMENT)
                            .await;
                        self.fire_error(session_id, LifecyclePhase::Submit, &err)
                            .await;
                        return match settlement {
                            Ok(()) => Err(err),
                            Err(settle_err) => Err(settle_err),
                        };
                    }

                    tool_cost = tool_cost.saturating_add(&output.cost);
                    tool_receipts.push(ToolCallReceipt {
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        arguments_digest: Sha256Digest::of(
                            &serde_json::to_vec(&call.arguments).unwrap_or_default(),
                        ),
                        output_digest: Sha256Digest::of(
                            &serde_json::to_vec(&output.content).unwrap_or_default(),
                        ),
                        cost: output.cost,
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
                        cost: RuntimeCostTuple::default(),
                    });
                }
            }

            // #359: recheck after tool.invoke awaits — a timeout during tool
            // execution would otherwise fall through to cost finalize + commit.
            let known_incurred = response.cost.saturating_add(&tool_cost);
            reservation = match self
                .abort_if_caller_gone(session_id, reservation, known_incurred, &cancel_probe)
                .await
            {
                Ok(reservation) => reservation,
                Err(RuntimeError::TurnCancelled) => {
                    self.record_turn_cancellation(session_id, &claims, committed_rounds)
                        .await;
                    return Err(RuntimeError::TurnCancelled);
                }
                Err(err) => return Err(err),
            };

            // 6. receipt: mint over the provider response, recording the tool
            //    calls and the combined (provider + tool) cost, chained onto the
            //    prior receipt. The commit lock covers parent-hash selection,
            //    signing, cost finalization, journal append, receipt persistence,
            //    and tail update; this prevents concurrent turns from forking the
            //    receipt chain or rolling back each other's journals.
            let combined_cost = response.cost.saturating_add(&tool_cost);
            let body = ReceiptBody {
                receipt_id: uuid::Uuid::new_v4(),
                parent_hash: None,
                verb: self.verb.clone(),
                issued_at: ardur_receipt::UnixTsMillis(iteration_now_ms),
                subject: ardur_receipt::HolderId(claims.subject.0.clone()),
                cap_token_id: ardur_receipt::TokenId(claims.token_id),
                payload_digest: Sha256Digest::of(response.content.as_bytes()),
                session_id: Some(session_id.0),
                cost: combined_cost,
                tool_calls: tool_receipts,
                provider: Some(self.provider.name()),
            };
            let (signed, receipt) = match self
                .commit_round(
                    &mut reservation,
                    body,
                    &claims,
                    !wants_tools,
                    iteration,
                    &req,
                    &response,
                    committed.as_ref(),
                    cancel_probe.as_ref(),
                )
                .await
            {
                Ok(result) => result,
                Err(err) => {
                    self.fire_error(session_id, LifecyclePhase::Receipt, &err)
                        .await;
                    if matches!(err, RuntimeError::TurnCancelled) {
                        self.record_turn_cancellation(session_id, &claims, committed_rounds)
                            .await;
                    }
                    return Err(err);
                }
            };

            // Final economics and projections are closed. A parked observer
            // must not prevent a queued healthy turn from being admitted.
            if !wants_tools {
                economic_permit.take();
            }

            // 8. post-receipt hooks (observational; the call already happened).
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
                let memory_now_ms = self.clock.now_ms().get();
                let memory_now_unix = memory_now_ms / 1000;
                let memory_write_claims = CapToken::from_base64(&req.cap_token.0, &self.cap_root)
                    .and_then(|token| {
                        self.verifier.verify(
                            &token,
                            &self.cap_root,
                            &RequiredCaveats {
                                now_unix: memory_now_unix,
                                audience,
                                tool: ardur_memory::MEMORY_WRITE_CAPABILITY.to_string(),
                                cost: self.cost_units,
                            },
                        )
                    });
                match memory_write_claims {
                    Ok(mem_claims) => {
                        let record = turn_record(
                            &mem_claims.subject.0,
                            &response,
                            &receipt,
                            iteration_now_ms,
                        );
                        // #543: the memory write is an evaluated event — its
                        // record digest is the content-addressed evidence, and
                        // the pre record lands BEFORE the backend write is
                        // invoked, so a crash mid-write never leaves a durable
                        // mutation without its journal record.
                        let record_digest =
                            Sha256Digest::of(&serde_json::to_vec(&record).unwrap_or_default())
                                .to_hex();
                        let memory_pre = self.governance_memory_pre_effect(
                            session_id,
                            &receipt,
                            &claims,
                            &record_digest,
                        );
                        let plane = MemoryControlPlane::new(memory.as_ref(), self.policies.clone());
                        match plane.record(&mem_claims, record) {
                            Ok(_record_id) => {
                                self.governance_terminal_event(
                                    &memory_pre,
                                    EventOutcome::Completed(CompletedOutcome {
                                        output_digest: record_digest.clone(),
                                        cost: RuntimeCostTuple::default(),
                                        output_admission: EvidenceOutputAdmission::Allowed,
                                    }),
                                );
                            }
                            Err(mem_err) => {
                                // #543: a control-plane rejection is a typed
                                // denial; an operational failure leaves the
                                // effect unknown.
                                self.governance_terminal_event(
                                    &memory_pre,
                                    memory_failure_outcome(&mem_err),
                                );
                                self.fire_error(session_id, LifecyclePhase::MemoryWrite, &mem_err)
                                    .await;
                            }
                        }
                    }
                    Err(cap_err) => {
                        // #543: the re-verification denial is itself an
                        // evaluated event — recorded through the §9 fail-closed
                        // mapping, never guessed.
                        let denied = match ErAuthOutcome::from_cap_token_error(&cap_err) {
                            ErAuthOutcome::Violation { public, internal } => {
                                DeniedOutcome { public, internal }
                            }
                            ErAuthOutcome::InsufficientEvidence { internal } => DeniedOutcome {
                                public: ErPublicDenialReason::InsufficientEvidence,
                                internal,
                            },
                            ErAuthOutcome::Compliant => DeniedOutcome {
                                public: ErPublicDenialReason::PolicyDenied,
                                internal: "memory_write_denied".to_string(),
                            },
                        };
                        self.governance_memory_denied_event(
                            session_id,
                            &receipt,
                            &claims,
                            "",
                            denied.public,
                            &denied.internal,
                        );
                        match cap_err {
                            CapTokenError::ToolNotAllowed => {
                                // Token does not grant memory.write (attenuated
                                // or never issued). Skip the memory side-effect
                                // silently — this is the intended behaviour for
                                // write-less tokens.
                            }
                            CapTokenError::Expired => {
                                let err = RuntimeError::CapTokenExpired;
                                tracing::warn!(
                                    session_id = ?session_id,
                                    error = %err,
                                    "memory.write cap-token re-verification failed"
                                );
                                self.fire_error(session_id, LifecyclePhase::MemoryWrite, &err)
                                    .await;
                            }
                            other => {
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
                }
            }

            // 10. session-journal + receipt were committed atomically before
            //     post-receipt hooks/finalize/memory, so no separate journal append
            //     runs here.

            total_cost = total_cost.checked_add(&combined_cost).ok_or_else(|| {
                RuntimeError::Internal(anyhow::anyhow!(
                    "settlement cumulative turn cost overflow; paid rounds retained"
                ))
            })?;
            committed_rounds += 1;

            // Termination: a response with no tool calls is the final answer; a
            // tool-wanting response at the iteration ceiling aborts; otherwise we
            // fold the assistant's tool-call turn and the tool results into the
            // transcript and loop.
            if !wants_tools {
                break (receipt, response.content);
            }
            if exhausted {
                reservation
                    .owner
                    .finish_refused(RefusalClass::IterationLimit)
                    .map_err(settlement_error)?;
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
    /// **Cancellation.** Owning Drop synchronously refunds an undecided round
    /// and retains known provider/tool expense in the authoritative settlement
    /// snapshot (or visible supervised uncertainty on storage failure). It never
    /// runs an executor. Retain [`Self::settlement_supervisor`] and explicitly
    /// drive its bounded journal drain before reopening journal projections.
    /// Earlier paid tool rounds remain paid; cancellation is not a final answer.
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
            let now_ms = self.clock.now_ms().get();
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
            let mut economic_permit = Some(self.economic_admission.lock().await);
            let mut owner = self.begin_settlement(session_id, &claims, &provisioning)?;
            let gate_token_id = match self.stage_cost_setup(&claims, &provisioning).await {
                Ok(gate_token_id) => gate_token_id,
                Err(err) => {
                    self.fire_error(session_id, LifecyclePhase::Submit, &err).await;
                    Err(err)?
                }
            };

            // gh#415: same cap-token narrowing as the non-streaming path.
            let tool_defs = self.tool_defs_for(&req, &provisioning, session_id, now_unix);

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
                    GateSha256::of(&serde_json::to_vec(&iter_request).map_err(|e| RuntimeError::Internal(e.into()))?);
                let reservation_handle = match self
                    .admit_round(&mut owner, AdmissionRequest {
                        cap_token_id: gate_token_id,
                        projected_envelope: self.envelope,
                        provider_id: self.gate_provider_id.clone(),
                        model_id: self.gate_model_id.clone(),
                        request_digest,
                    }, iter_request.request_id.0)
                    .await
                {
                    Ok(reservation) => reservation,
                    Err(e) => {
                        let err = e;
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
                let reservation_id = reservation_handle.reservation_id;
                let mut reservation = Some(reservation_handle);
                // A yield can drop the owning generator immediately. Install
                // reservation cleanup before acknowledging admission, including
                // the no-provider-work boundary.
                yield FusedEvent::StageEnd { stage: StageKind::CostGateAdmit, ok: true };

                // 5. provider stream: forward each delta as it arrives.
                yield FusedEvent::StageStart { stage: StageKind::ProviderStream };
                reservation.as_mut().expect("held").owner.observe_provider(ProviderEvidence::DispatchIntent).map_err(settlement_error)?;
                // #543: keep the round's request id — the tool-event identity
                // discriminator — past the request's move.
                let iter_request_id = iter_request.request_id.0.to_string();
                let mut provider_stream = match self.provider.stream(iter_request).await {
                    Ok(provider_stream) => provider_stream,
                    Err(provider_err) => {
                        self.release_failure(reservation.take().expect("reservation held")).await;
                        yield FusedEvent::StageEnd { stage: StageKind::ProviderStream, ok: false };
                        self.fire_error(session_id, LifecyclePhase::Provider, &provider_err)
                            .await;
                        Err(map_provider_error(&provider_err))?
                    }
                };
                let mut content = String::new();
                let mut usage = Usage::default();
                let mut saw_usage = false;
                let mut finish_reason = FinishReason::Stop;
                let mut stream_err: Option<ProviderError> = None;
                while let Some(item) = provider_stream.next().await {
                    // ARD-501: the reservation was admitted with a TTL sized for
                    // a prompt turn; a long generation can outlive it. Refresh
                    // the lease on every provider event so an actively-streaming
                    // turn is never reclaimed as abandoned and discarded at
                    // `finalize`. Cheap, lock-only, and a no-op once the
                    // reservation is being finalized.
                    self.gate.touch_reservation(reservation_id);
                    match item {
                        Ok(StreamEvent::ContentDelta(text)) => {
                            content.push_str(&text);
                            // ARD-491: cap accumulated streamed content so an
                            // adversarial/buggy provider can't drive this buffer
                            // (and the receipt/memory/journal chain it feeds) to
                            // unbounded size. Fail closed: abort the turn, no
                            // partial response enters the auditable chain.
                            if content.len() > self.stream_content_max_bytes {
                                self.release_failure(
                                    reservation.take().expect("reservation held"),
                                )
                                .await;
                                let err = RuntimeError::StreamedContentCapExceeded {
                                    limit: self.stream_content_max_bytes,
                                    actual: content.len(),
                                };
                                yield FusedEvent::StageEnd {
                                    stage: StageKind::ProviderStream,
                                    ok: false,
                                };
                                self.fire_error(session_id, LifecyclePhase::Provider, &err)
                                    .await;
                                Err(err)?;
                            }
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
                        Ok(StreamEvent::Usage(reported)) => {
                            self.observe_usage(reservation.as_mut().expect("held"), reported, false)?;
                            usage = reported;
                            saw_usage = true;
                        },
                        Ok(StreamEvent::Finish(reason)) => finish_reason = reason,
                        Ok(StreamEvent::ServedModel(model)) => {
                            // Record the actual model served (ARD-454).
                            // The instrument layer picks this up from the
                            // response model attribute set below.
                            _ = model;
                        }
                        Err(provider_err) => {
                            stream_err = Some(provider_err);
                            break;
                        }
                    }
                }
                // Free the provider stream before the post-provider stages run.
                drop(provider_stream);
                if let Some(provider_err) = stream_err {
                    self.release_failure(reservation.take().expect("reservation held")).await;
                    yield FusedEvent::StageEnd { stage: StageKind::ProviderStream, ok: false };
                    self.fire_error(session_id, LifecyclePhase::Provider, &provider_err)
                        .await;
                    Err(map_provider_error(&provider_err))?;
                }
                if !saw_usage {
                    self.release_failure(reservation.take().expect("held")).await;
                    Err(RuntimeError::ProviderUnavailable)?;
                }
                self.observe_usage(reservation.as_mut().expect("held"), usage, true)?;
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
                if requested.len() > crate::settlement::LIMITS.max_tools_per_round {
                    let err = RuntimeError::Internal(anyhow::anyhow!(
                        "tool batch exceeds settlement capacity: {} > {}",
                        requested.len(), crate::settlement::LIMITS.max_tools_per_round
                    ));
                    let settlement = self.settle_refusal(
                        session_id, reservation.take().expect("reservation held"),
                        response.cost, TOOL_CAPACITY_SETTLEMENT
                    ).await;
                    self.fire_error(session_id, LifecyclePhase::Provider, &err).await;
                    settlement?;
                    Err(err)?;
                    unreachable!()
                }
                let wants_tools = !requested.is_empty();
                let exhausted = wants_tools && iteration >= self.max_tool_iterations;

                // 6. tool execution.
                let mut tool_receipts: Vec<ToolCallReceipt> = Vec::new();
                let mut tool_messages: Vec<ChatMessage> = Vec::new();
                let mut tool_cost = RuntimeCostTuple::default();
                if wants_tools && !exhausted {
                    yield FusedEvent::StageStart { stage: StageKind::ToolExec };
                    for (tool_ordinal, call) in requested.iter().enumerate() {
                        let Some(tool) = self.tools.get(&ToolId::new(&call.name)) else {
                            let err = RuntimeError::UnknownTool {
                                tool: call.name.clone(),
                            };
                            // #543: evaluated and rejected (no registered
                            // tool) — this round never commits.
                            self.governance_denied_event(
                                session_id,
                                &iter_request_id,
                                iteration,
                                tool_ordinal,
                                call,
                                &claims,
                                &[],
                                ErPublicDenialReason::PolicyDenied,
                                "unknown_tool",
                            );
                            yield FusedEvent::StageEnd { stage: StageKind::ToolExec, ok: false };
                            let known = response.cost.saturating_add(&tool_cost);
                            let settlement = self
                                .settle_refusal(
                                    session_id,
                                    reservation.take().expect("reservation held"),
                                    known,
                                    UNKNOWN_TOOL_SETTLEMENT,
                                )
                                .await;
                            // Observe the original refusal even if accounting failed;
                            // the settlement error still takes precedence for callers.
                            self.fire_error(session_id, LifecyclePhase::Provider, &err).await;
                            settlement?;
                            Err(err)?;
                            unreachable!()
                        };
                        let invocation_now_unix = self.clock.now_ms().get() / 1000;
                        if let Err(err) = self.authorize_tool_invocation(
                            &req,
                            &provisioning,
                            session_id,
                            invocation_now_unix,
                            &call.name,
                        ) {
                            // #543: cap-token/Cedar tool-invoke denial.
                            let (public, internal) = tool_auth_denial_classification(&err);
                            self.governance_denied_event(
                                session_id,
                                &iter_request_id,
                                iteration,
                                tool_ordinal,
                                call,
                                &claims,
                                tool.required_capabilities(),
                                public,
                                internal,
                            );
                            yield FusedEvent::StageEnd { stage: StageKind::ToolExec, ok: false };
                            let known = response.cost.saturating_add(&tool_cost);
                            let settlement = self
                                .settle_refusal(
                                    session_id,
                                    reservation.take().expect("reservation held"),
                                    known,
                                    TOOL_AUTH_SETTLEMENT,
                                )
                                .await;
                            self.fire_error(session_id, LifecyclePhase::Submit, &err).await;
                            settlement?;
                            Err(err)?;
                            unreachable!()
                        }

                        // ARD-420: enforce required_capabilities() against the
                        // cap-token's tool allowlist before the tool body runs.
                        if let Err(err) = self.authorize_tool_capabilities(
                            &req,
                            &provisioning,
                            invocation_now_unix,
                            &call.name,
                            tool.required_capabilities(),
                        ) {
                            // #543: declared-capability denial.
                            self.governance_denied_event(
                                session_id,
                                &iter_request_id,
                                iteration,
                                tool_ordinal,
                                call,
                                &claims,
                                tool.required_capabilities(),
                                ErPublicDenialReason::PolicyDenied,
                                "capability_not_granted",
                            );
                            yield FusedEvent::StageEnd { stage: StageKind::ToolExec, ok: false };
                            let known = response.cost.saturating_add(&tool_cost);
                            let settlement = self
                                .settle_refusal(
                                    session_id,
                                    reservation.take().expect("reservation held"),
                                    known,
                                    CAPABILITY_SETTLEMENT,
                                )
                                .await;
                            self.fire_error(session_id, LifecyclePhase::Submit, &err).await;
                            settlement?;
                            Err(err)?;
                            unreachable!()
                        }

                        // ARD-139: a call whose required capabilities include
                        // an approval-gated one needs human sign-off even
                        // though the cap-token/cedar checks above already
                        // allow it. gh#497: only a WON claim (bound to this
                        // exact call and stamped with the verified caller)
                        // lets the invocation proceed.
                        let spent_approval = match self
                            .authorize_or_propose_approval(
                                &req.cap_token,
                                session_id,
                                invocation_now_unix,
                                &call.name,
                                tool.required_capabilities(),
                                &call.arguments,
                                &claims.subject.0,
                            )
                            .await
                        {
                            Ok(spent) => spent,
                            Err(err) => {
                                // #543: approval-gate denial (classified: an
                                // operational failure of the approval gate is
                                // insufficient_evidence, never a violation).
                                let (public, internal) = approval_denial_classification(&err);
                                self.governance_denied_event(
                                    session_id,
                                    &iter_request_id,
                                    iteration,
                                    tool_ordinal,
                                    call,
                                    &claims,
                                    tool.required_capabilities(),
                                    public,
                                    internal,
                                );
                                yield FusedEvent::StageEnd { stage: StageKind::ToolExec, ok: false };
                                let known = response.cost.saturating_add(&tool_cost);
                                let reason = if matches!(err, RuntimeError::ApprovalRequired { .. }) {
                                    APPROVAL_SETTLEMENT
                                } else {
                                    APPROVAL_REJECTED_SETTLEMENT
                                };
                                let settlement = self
                                    .settle_refusal(
                                        session_id,
                                        reservation.take().expect("reservation held"),
                                        known,
                                        reason,
                                    )
                                    .await;
                                self.fire_error(session_id, LifecyclePhase::Submit, &err).await;
                                settlement?;
                                Err(err)?;
                                unreachable!()
                            }
                        };
                        let ctx = self.tool_context(&req.cap_token, session_id);
                        // #543: gates passed. Observe the dispatch intent
                        // first (it is fallible), then persist the durable
                        // inputs AFTER it succeeds and BEFORE the effect — an
                        // observation failure leaves no stranded pre the sweep
                        // would misread as `effect_unobserved`.
                        self.observe_tool(reservation.as_mut().expect("held"), tool_ordinal, call, ToolEffect::DispatchIntent, OutputAdmission::NotScanned)?;
                        let governance_pre = self.governance_pre_effect(
                            session_id,
                            &iter_request_id,
                            iteration,
                            tool_ordinal,
                            call,
                            &claims,
                            tool.required_capabilities(),
                        );
                        let (output, scan_result) = match tokio::time::timeout(
                            self.tool_timeout,
                            tool.invoke(&ctx, call.arguments.clone()),
                        )
                        .await
                        {
                            Ok(Ok(output)) => {
                                // #543: scan, terminal evidence, then the
                                // fallible settlement observation — an
                                // already-observed result is never stranded
                                // behind a persist failure.
                                let scan_result = self.scan_tool_output(&call.name, &output.content).await;
                                self.governance_terminal_event(
                                    &governance_pre,
                                    EventOutcome::Completed(CompletedOutcome {
                                        output_digest: Sha256Digest::of(
                                            &serde_json::to_vec(&output.content).unwrap_or_default(),
                                        )
                                        .to_hex(),
                                        cost: output.cost,
                                        output_admission: match &scan_result {
                                            Ok(()) => EvidenceOutputAdmission::Allowed,
                                            Err(RuntimeError::InjectionBlocked { .. }) => {
                                                EvidenceOutputAdmission::Blocked
                                            }
                                            Err(_) => EvidenceOutputAdmission::Undetermined,
                                        },
                                    }),
                                );
                                self.observe_tool(reservation.as_mut().expect("held"), tool_ordinal, call, ToolEffect::Completed {
                                    output_digest: GateSha256::of(&serde_json::to_vec(&output.content).map_err(|e| RuntimeError::Internal(e.into()))?),
                                    cost: output.cost,
                                }, if scan_result.is_ok() { OutputAdmission::Allowed } else { OutputAdmission::Blocked })?;
                                self.record_approval_invocation(
                                    &spent_approval,
                                    InvocationResult::Completed,
                                );
                                (output, scan_result)
                            }
                            Ok(Err(tool_err)) => {
                                // #543: terminal evidence BEFORE the fallible
                                // settlement observation — and typed
                                // pre-effect refusals record as canonical
                                // denials, not unknown effects.
                                self.governance_terminal_event(
                                    &governance_pre,
                                    Self::tool_error_event_outcome(&tool_err),
                                );
                                self.observe_tool(reservation.as_mut().expect("held"), tool_ordinal, call, ToolEffect::Failed { class: ToolFailureClass::Execution, effect_unknown: true }, OutputAdmission::NotScanned)?;
                                // Consume-before-invoke: the approval stays
                                // spent; the card records the call resolved
                                // as failed.
                                self.record_approval_invocation(&spent_approval, InvocationResult::Failed);
                                let err = map_tool_error(tool_err, &call.name);
                                yield FusedEvent::StageEnd { stage: StageKind::ToolExec, ok: false };
                                let known = response.cost.saturating_add(&tool_cost);
                                let settlement = self
                                    .settle_refusal(
                                        session_id,
                                        reservation.take().expect("reservation held"),
                                        known,
                                        TOOL_ERROR_SETTLEMENT,
                                    )
                                    .await;
                                self.fire_error(session_id, LifecyclePhase::Provider, &err).await;
                                settlement?;
                                Err(err)?;
                                unreachable!()
                            }
                            Err(_elapsed) => {
                                // #543: timeout with a possible effect —
                                // terminal evidence BEFORE the fallible
                                // settlement observation.
                                self.governance_terminal_event(
                                    &governance_pre,
                                    EventOutcome::TimeoutUnknown,
                                );
                                self.observe_tool(reservation.as_mut().expect("held"), tool_ordinal, call, ToolEffect::InterruptedUnknown, OutputAdmission::NotScanned)?;
                                // Timed out with the invocation's effect
                                // unknown: the spent card is deliberately
                                // left WITHOUT an outcome record — the
                                // explicit ambiguous-effect state. The
                                // provider round is KNOWN and debited; the
                                // timed-out tool's cost is not (gh#452).
                                let known = response.cost.saturating_add(&tool_cost);
                                let reservation = reservation.take().expect("reservation held");
                                let settlement = self
                                    .settle_refusal(
                                        session_id,
                                        reservation,
                                        known,
                                        TOOL_TIMEOUT_SETTLEMENT,
                                    )
                                    .await;
                                let err = RuntimeError::ToolTimeout {
                                    tool: call.name.clone(),
                                };
                                yield FusedEvent::StageEnd { stage: StageKind::ToolExec, ok: false };
                                self.fire_error(session_id, LifecyclePhase::Provider, &err).await;
                                settlement?;
                                Err(err)?;
                                unreachable!()
                            }
                        };

                        // The scan decision and its terminal evidence were
                        // made in the success arm above (before the first
                        // fallible settlement observation); only the
                        // admission outcome is handled here.
                        if let Err(err) = scan_result {
                            yield FusedEvent::StageEnd { stage: StageKind::ToolExec, ok: false };
                            let known = response
                                .cost
                                .saturating_add(&tool_cost)
                                .saturating_add(&output.cost);
                            let settlement = self
                                .settle_refusal(
                                    session_id,
                                    reservation.take().expect("reservation held"),
                                    known,
                                    SCAN_SETTLEMENT,
                                )
                                .await;
                            self.fire_error(session_id, LifecyclePhase::Submit, &err).await;
                            settlement?;
                            Err(err)?;
                        }

                        yield FusedEvent::ToolCallResult {
                            id: call.id.clone(),
                            result: output.content.clone(),
                        };

                        tool_cost = tool_cost.saturating_add(&output.cost);
                        tool_receipts.push(ToolCallReceipt {
                            call_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            arguments_digest: Sha256Digest::of(
                                &serde_json::to_vec(&call.arguments).unwrap_or_default(),
                            ),
                            output_digest: Sha256Digest::of(
                                &serde_json::to_vec(&output.content).unwrap_or_default(),
                            ),
                            cost: output.cost,
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
                            cost: RuntimeCostTuple::default(),
                        });
                    }
                }

                // 7. receipt mint + chain.
                yield FusedEvent::StageStart { stage: StageKind::ReceiptMint };
                let combined_cost = response.cost.saturating_add(&tool_cost);
                // Settle the provider-plus-tool actual before any receipt or
                // journal state becomes durable. A rejected/expired reservation
                // therefore cannot produce authoritative spend evidence.
                yield FusedEvent::StageStart { stage: StageKind::CostGateFinalize };
                // gh#498: match the non-streaming atomicity contract —
                // serialize parent-tail selection, cost settlement, receipt
                // persistence, and journal append — WITHOUT holding the commit
                // lock across a consumer-visible yield. Every yield below the
                // lock used to park ALL other turns' commits for as long as
                // this stream's consumer stayed parked; the guarded block is
                // now yield-free and the stage events are emitted after the
                // guard drops, in the same consumer-visible order.
                let body = ReceiptBody {
                    receipt_id: uuid::Uuid::new_v4(), parent_hash: None, verb: self.verb.clone(),
                    issued_at: ardur_receipt::UnixTsMillis(now_ms),
                    subject: ardur_receipt::HolderId(claims.subject.0.clone()),
                    cap_token_id: ardur_receipt::TokenId(claims.token_id),
                    payload_digest: Sha256Digest::of(response.content.as_bytes()),
                    session_id: Some(session_id.0), cost: combined_cost,
                    tool_calls: tool_receipts, provider: Some(self.provider.name()),
                };
                let (signed, receipt) = match self.commit_round(reservation.as_mut().expect("held"), body,
                    &claims, !wants_tools, iteration, &req, &response, None, None).await {
                    Ok(result) => result,
                    Err(err) => {
                        self.fire_error(session_id, LifecyclePhase::Receipt, &err).await;
                        yield FusedEvent::StageEnd { stage: StageKind::CostGateFinalize, ok: false };
                        yield FusedEvent::StageEnd { stage: StageKind::ReceiptMint, ok: false };
                        Err(err)?;
                        unreachable!()
                    }
                };
                // No economic capacity retained at the final Receipt yield.
                if !wants_tools {
                    economic_permit.take();
                }
                yield FusedEvent::StageEnd { stage: StageKind::CostGateFinalize, ok: true };
                let chain_hash = Sha256Digest::of(signed.jws_compact().as_bytes());
                yield FusedEvent::Receipt {
                    receipt_id: ReceiptId(receipt.receipt_id),
                    chain_hash: format!("{chain_hash}"),
                    cost_cents: combined_cost.cents,
                };
                yield FusedEvent::StageEnd { stage: StageKind::ReceiptMint, ok: true };

                // 8. post-receipt hooks (observational).
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
                    let memory_now_ms = self.clock.now_ms().get();
                    let memory_now_unix = memory_now_ms / 1000;
                    let memory_write_claims = CapToken::from_base64(&req.cap_token.0, &self.cap_root)
                        .and_then(|token| {
                            self.verifier.verify(
                                &token,
                                &self.cap_root,
                                &RequiredCaveats {
                                    now_unix: memory_now_unix,
                                    audience,
                                    tool: ardur_memory::MEMORY_WRITE_CAPABILITY.to_string(),
                                    cost: self.cost_units,
                                },
                            )
                        });
                    match memory_write_claims {
                        Ok(mem_claims) => {
                            let record = turn_record(
                                &mem_claims.subject.0,
                                &response,
                                &receipt,
                                memory_now_ms,
                            );
                            // #543: the memory write is an evaluated event —
                            // the pre record lands BEFORE the backend write.
                            let record_digest = Sha256Digest::of(
                                &serde_json::to_vec(&record).unwrap_or_default(),
                            )
                            .to_hex();
                            let memory_pre = self.governance_memory_pre_effect(
                                session_id,
                                &receipt,
                                &claims,
                                &record_digest,
                            );
                            let plane = MemoryControlPlane::new(memory.as_ref(), self.policies.clone());
                            match plane.record(&mem_claims, record) {
                                Ok(_record_id) => {
                                    self.governance_terminal_event(
                                        &memory_pre,
                                        EventOutcome::Completed(CompletedOutcome {
                                            output_digest: record_digest.clone(),
                                            cost: RuntimeCostTuple::default(),
                                            output_admission: EvidenceOutputAdmission::Allowed,
                                        }),
                                    );
                                }
                                Err(mem_err) => {
                                    // #543: control-plane denial vs unknown effect.
                                    self.governance_terminal_event(
                                        &memory_pre,
                                        memory_failure_outcome(&mem_err),
                                    );
                                    self.fire_error(session_id, LifecyclePhase::MemoryWrite, &mem_err)
                                        .await;
                                }
                            }
                        }
                        Err(cap_err) => {
                            // #543: the re-verification denial is itself an
                            // evaluated event (§9 fail-closed mapping).
                            let denied = match ErAuthOutcome::from_cap_token_error(&cap_err) {
                                ErAuthOutcome::Violation { public, internal } => {
                                    DeniedOutcome { public, internal }
                                }
                                ErAuthOutcome::InsufficientEvidence { internal } => DeniedOutcome {
                                    public: ErPublicDenialReason::InsufficientEvidence,
                                    internal,
                                },
                                ErAuthOutcome::Compliant => DeniedOutcome {
                                    public: ErPublicDenialReason::PolicyDenied,
                                    internal: "memory_write_denied".to_string(),
                                },
                            };
                            self.governance_memory_denied_event(
                                session_id,
                                &receipt,
                                &claims,
                                "",
                                denied.public,
                                &denied.internal,
                            );
                            match cap_err {
                                CapTokenError::ToolNotAllowed => {
                                    // Token does not grant memory.write — skip silently.
                                }
                                CapTokenError::Expired => {
                                    let err = RuntimeError::CapTokenExpired;
                                    tracing::warn!(
                                        session_id = ?session_id,
                                        error = %err,
                                        "memory.write cap-token re-verification failed"
                                    );
                                    self.fire_error(session_id, LifecyclePhase::MemoryWrite, &err)
                                        .await;
                                }
                                other => {
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
                    reservation.as_mut().expect("held").owner.finish_refused(RefusalClass::IterationLimit).map_err(settlement_error)?;
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

/// **#543.** Classify an invoked tool from its **declared** capability
/// surface — the authorized intent, never the argument values (classifying
/// from arguments would be guessing at their semantics). For a mixed
/// capability set the strongest declared effect wins, in a fixed severity
/// order, so the classification is deterministic per tool; the fold seeds
/// from the first declared capability so an all-read set classifies as a
/// read rather than the no-capability default. A tool declaring no
/// capabilities classifies as an observation with no side effect (the
/// registry's Phase-1 tools, e.g. `echo`, match that); a `Custom` capability
/// declares nothing, so it classifies conservatively as state-changing
/// rather than under-claim an unknown effect surface.
fn classify_tool_invocation(tool_name: &str, required: &[Capability]) -> InvocationClassification {
    let (mut family, mut action, mut side) = match required.first() {
        Some(cap) => capability_effect(cap),
        None => ("tool", ErActionClass::Observe, ErSideEffectClass::None),
    };
    for cap in required {
        let (cap_family, cap_action, cap_side) = capability_effect(cap);
        if side_effect_severity(cap_side) > side_effect_severity(side) {
            side = cap_side;
            action = cap_action;
            family = cap_family;
        }
    }
    InvocationClassification {
        action_class: action,
        target: tool_name.to_string(),
        resource_family: family.to_string(),
        side_effect_class: side,
    }
}

/// The severity order the #543 classification maximizes over: an external
/// send outranks a durable state change, which outranks an internal write,
/// which outranks no effect.
fn side_effect_severity(side: ErSideEffectClass) -> u8 {
    match side {
        ErSideEffectClass::None => 0,
        ErSideEffectClass::InternalWrite => 1,
        ErSideEffectClass::StateChange => 2,
        ErSideEffectClass::ExternalSend => 3,
    }
}

/// The (resource family, action class, side-effect class) one declared
/// capability contributes to the #543 classification.
fn capability_effect(cap: &Capability) -> (&'static str, ErActionClass, ErSideEffectClass) {
    match cap {
        Capability::FsRead => ("filesystem", ErActionClass::Read, ErSideEffectClass::None),
        Capability::FsWrite => (
            "filesystem",
            ErActionClass::Write,
            ErSideEffectClass::StateChange,
        ),
        Capability::ShellExec => (
            "process",
            ErActionClass::Write,
            ErSideEffectClass::StateChange,
        ),
        Capability::ProcessSpawn => (
            "process",
            ErActionClass::Write,
            ErSideEffectClass::StateChange,
        ),
        Capability::NetworkOut => (
            "network",
            ErActionClass::Send,
            ErSideEffectClass::ExternalSend,
        ),
        Capability::EnvRead => ("process", ErActionClass::Read, ErSideEffectClass::None),
        Capability::ClipboardRead => ("device", ErActionClass::Read, ErSideEffectClass::None),
        Capability::VoiceInput => ("device", ErActionClass::Observe, ErSideEffectClass::None),
        Capability::VoiceOutput => (
            "device",
            ErActionClass::Write,
            ErSideEffectClass::InternalWrite,
        ),
        Capability::ImageGenerate => (
            "model",
            ErActionClass::Write,
            ErSideEffectClass::InternalWrite,
        ),
        Capability::ImageAnalyze => ("model", ErActionClass::Observe, ErSideEffectClass::None),
        Capability::Custom(_) => ("tool", ErActionClass::Write, ErSideEffectClass::StateChange),
    }
}

/// **#543.** The (public reason, audit code) pair for a tool-invocation gate
/// denial, mapped from the runtime's typed error. A Cedar **indeterminate**
/// (an evaluation error, which `stage_cedar_with_action` surfaces as
/// `PolicyDenied` with an `indeterminate:` reason) establishes neither
/// permission nor a breach — it is `insufficient_evidence`, never a proven
/// violation. Any other operational failure classifies the same way: only
/// the recognized policy-denial shapes mint a violation.
///
/// `stage_cap_token_for_tool_at` collapses the typed [`CapTokenError`] into
/// `RuntimeError::CapDenied`'s display string; the classifications the
/// mirror distinguishes are re-derived from those exact display strings
/// (constructed from the enum itself, so a display change breaks the unit
/// tests loudly rather than silently mis-classifying). This preserves the
/// repository's existing `revoked`/`revoked` classification for mid-turn
/// revocations instead of flattening them into `tool_not_allowed`.
fn tool_auth_denial_classification(err: &RuntimeError) -> (ErPublicDenialReason, &'static str) {
    match err {
        RuntimeError::CapTokenExpired => (ErPublicDenialReason::PolicyDenied, "grant_expired"),
        RuntimeError::CapDenied { reason } => {
            if *reason == CapTokenError::Revoked.to_string() {
                (ErPublicDenialReason::Revoked, "revoked")
            } else if *reason == CapTokenError::AudienceMismatch.to_string() {
                (ErPublicDenialReason::PolicyDenied, "audience_mismatch")
            } else if *reason == CapTokenError::SignatureInvalid.to_string() {
                (ErPublicDenialReason::ChainInvalid, "signature_invalid")
            } else if *reason == CapTokenError::BudgetExhausted.to_string() {
                (ErPublicDenialReason::BudgetExhausted, "budget_exhausted")
            } else {
                (ErPublicDenialReason::PolicyDenied, "tool_not_allowed")
            }
        }
        RuntimeError::PolicyDenied { reason } if reason.starts_with("indeterminate:") => (
            ErPublicDenialReason::InsufficientEvidence,
            "policy_indeterminate",
        ),
        RuntimeError::PolicyDenied { .. } => (ErPublicDenialReason::PolicyDenied, "policy_denied"),
        _ => (
            ErPublicDenialReason::InsufficientEvidence,
            "tool_invocation_error",
        ),
    }
}

/// **#543.** The (public reason, audit code) pair for an approval-gate
/// denial. Only an explicit human decision mints a violation; any
/// operational failure of the approval store/evaluation (store errors,
/// unrecognized claim outcomes, contention exhaustion) supports only
/// `insufficient_evidence` — no human rejection occurred.
fn approval_denial_classification(err: &RuntimeError) -> (ErPublicDenialReason, &'static str) {
    match err {
        RuntimeError::ApprovalRequired { .. } => {
            (ErPublicDenialReason::PolicyDenied, "approval_required")
        }
        RuntimeError::ApprovalRejected { .. } => {
            (ErPublicDenialReason::PolicyDenied, "approval_rejected")
        }
        _ => (
            ErPublicDenialReason::InsufficientEvidence,
            "approval_evaluation_error",
        ),
    }
}

/// **#543.** Classify a memory-write failure honestly: a control-plane
/// rejection (capability, policy, subject, receipt, or shape) happens BEFORE
/// the backend write, so the effect provably never occurred — a typed
/// denial. A policy *evaluation failure* (`PolicyIndeterminate`) establishes
/// neither permission nor a breach: `insufficient_evidence`, never a proven
/// violation. An operational failure (`Backend`, poisoned lock, missing
/// record) leaves the effect unknown: the write may have partially
/// completed.
fn memory_failure_outcome(err: &ardur_memory::MemoryError) -> EventOutcome {
    use ardur_memory::MemoryError as ME;
    let denied = |internal: &str| {
        EventOutcome::Denied(DeniedOutcome {
            public: ErPublicDenialReason::PolicyDenied,
            internal: internal.to_string(),
        })
    };
    match err {
        ME::CapabilityDenied { .. } => denied("memory_capability_denied"),
        ME::PolicyDenied { .. } => denied("memory_policy_denied"),
        ME::PolicyIndeterminate { .. } => EventOutcome::Denied(DeniedOutcome {
            public: ErPublicDenialReason::InsufficientEvidence,
            internal: "memory_policy_indeterminate".to_string(),
        }),
        ME::SubjectMismatch { .. } => denied("memory_subject_mismatch"),
        ME::ReceiptRequired { .. } => denied("memory_receipt_required"),
        ME::Malformed(_) => denied("memory_record_malformed"),
        ME::NotFound(_) | ME::LockPoisoned | ME::Backend(_) => EventOutcome::FailedUnknown,
    }
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

/// **§1.7.** A rough token-count estimate (~4 characters/token, the common
/// English-text heuristic) over a message slice's content — not a real
/// tokenizer. Good enough for `/compact status`-style before/after context
/// sizing; not billed anywhere (the receipted cost always comes from the
/// provider's actual reported [`Usage`](ardur_provider_runtime::Usage)).
fn estimate_tokens(messages: &[ChatMessage]) -> u64 {
    let chars: usize = messages.iter().map(|m| m.content.len()).sum();
    (chars as u64).div_ceil(4)
}

/// Map a provider failure onto the runtime's error surface.
fn map_provider_error(err: &ProviderError) -> RuntimeError {
    match err {
        ProviderError::CostCeilingExceeded => RuntimeError::CostCeilingExceeded,
        _ => RuntimeError::ProviderUnavailable,
    }
}

/// Preserve typed authorization denials and timeouts across the tool boundary.
/// Tool-local policy refusals are not cap-token denials; they and execution
/// faults retain the existing [`RuntimeError::Internal`] mapping.
fn map_tool_error(err: ToolError, tool: &str) -> RuntimeError {
    match err {
        ToolError::CapTokenDenied { reason } => RuntimeError::CapDenied { reason },
        ToolError::CapabilityDenied(capability) => RuntimeError::CapDenied {
            reason: format!(
                "tool `{tool}` requires capability `{}`",
                capability.as_str()
            ),
        },
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

/// A conservative context-injection score for the unscored `MemoryRuntime`
/// recall seam. Hybrid backends rank before returning hits; this final guard
/// filters weak lexical overlaps and low-confidence records before they enter
/// provider context.
fn memory_recall_score(query: &str, card: &MemoryCard) -> f32 {
    let terms = recall_terms(query);
    if terms.is_empty() {
        return 0.0;
    }
    let text = memory_payload_text(&card.payload).to_ascii_lowercase();
    let matched = terms
        .iter()
        .filter(|term| text.contains(term.as_str()))
        .count();
    let relevance = matched as f32 / terms.len() as f32;
    let confidence = card.confidence.unwrap_or(1.0) as f32;
    relevance.min(confidence.clamp(0.0, 1.0))
}

fn recall_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .filter(|term| !is_recall_stopword(term))
        .collect()
}

fn is_recall_stopword(term: &str) -> bool {
    matches!(
        term,
        "a" | "an"
            | "and"
            | "are"
            | "about"
            | "be"
            | "do"
            | "does"
            | "for"
            | "how"
            | "in"
            | "is"
            | "me"
            | "of"
            | "on"
            | "or"
            | "please"
            | "should"
            | "tell"
            | "the"
            | "to"
            | "we"
            | "what"
            | "when"
            | "where"
            | "why"
            | "with"
    )
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

#[cfg(test)]
mod tool_error_tests {
    use super::*;
    use ardur_tool_registry::Capability;

    #[test]
    fn tool_token_denial_stays_an_authorization_denial_without_reason_matching() {
        let err = map_tool_error(
            ToolError::CapTokenDenied {
                reason: "opaque verifier diagnostic".to_string(),
            },
            "probe",
        );
        assert!(
            matches!(err, RuntimeError::CapDenied { .. }),
            "typed token denial must preserve authorization-denial classification, got {err:?}"
        );
    }

    #[test]
    fn missing_tool_grant_stays_an_authorization_denial() {
        let err = map_tool_error(ToolError::CapabilityDenied(Capability::FsRead), "probe");
        assert!(
            matches!(err, RuntimeError::CapDenied { .. }),
            "missing tool grant must preserve authorization-denial classification, got {err:?}"
        );
    }

    #[test]
    fn tool_local_policy_denial_is_not_a_cap_token_denial() {
        // Authorization words in a tool-local reason must not change its type.
        let err = map_tool_error(
            ToolError::Denied {
                reason: "capability token denied: revoked".to_string(),
            },
            "probe",
        );
        assert!(
            matches!(err, RuntimeError::Internal(_)),
            "tool-local policy must not be relabeled as token authorization, got {err:?}"
        );
    }

    #[test]
    fn tool_timeout_and_execution_failure_keep_their_classes() {
        assert!(matches!(
            map_tool_error(ToolError::Timeout, "probe"),
            RuntimeError::ToolTimeout { tool } if tool == "probe"
        ));
        assert!(matches!(
            map_tool_error(ToolError::ExecutionFailed("revoked".to_string()), "probe"),
            RuntimeError::Internal(_)
        ));
    }
}

#[cfg(test)]
mod governance_classification_tests {
    //! #543 review hardening: the public/private classification of gate
    //! failures. An evaluation *failure* (Cedar indeterminate, scanner
    //! operational error, approval-store error) must never be filed as a
    //! proven violation — only `insufficient_evidence`.
    use super::*;

    #[test]
    fn every_tool_error_variant_classifies_exhaustively() {
        // The match in tool_error_event_outcome has no wildcard: a new
        // ToolError variant breaks this build until it is classified. This
        // pins each variant's class so a quiet reclassification is a test
        // failure, not a review surprise.
        use ardur_governance::{EventOutcome, PublicDenialReason};
        let cases: Vec<(ToolError, &str, PublicDenialReason)> = vec![
            (
                ToolError::Denied { reason: "r".into() },
                "tool_policy_denied",
                PublicDenialReason::PolicyDenied,
            ),
            (
                ToolError::CapabilityDenied(Capability::FsWrite),
                "tool_capability_denied",
                PublicDenialReason::PolicyDenied,
            ),
            (
                ToolError::CapTokenDenied { reason: "r".into() },
                "tool_cap_token_denied",
                PublicDenialReason::PolicyDenied,
            ),
            (
                ToolError::InvalidArgs("r".into()),
                "tool_invalid_arguments",
                PublicDenialReason::PolicyDenied,
            ),
            (
                ToolError::CostCeilingExceeded,
                "tool_cost_ceiling_exceeded",
                PublicDenialReason::BudgetExhausted,
            ),
            (
                ToolError::NotImplemented("r".into()),
                "tool_not_implemented",
                PublicDenialReason::InsufficientEvidence,
            ),
        ];
        for (err, code, public) in cases {
            match FusedRuntime::tool_error_event_outcome(&err) {
                EventOutcome::Denied(d) => {
                    assert_eq!(d.internal, code);
                    assert_eq!(d.public, public);
                }
                other => panic!("{code} must classify as a typed denial, got {other:?}"),
            }
        }
        // The execution-ambiguous variants stay unknown-effect.
        for err in [
            ToolError::ExecutionFailed("r".into()),
            ToolError::OutputTooLarge { actual: 9, max: 1 },
            ToolError::Internal(anyhow::anyhow!("r")),
        ] {
            assert!(matches!(
                FusedRuntime::tool_error_event_outcome(&err),
                EventOutcome::FailedUnknown
            ));
        }
        assert!(matches!(
            FusedRuntime::tool_error_event_outcome(&ToolError::Timeout),
            EventOutcome::TimeoutUnknown
        ));
    }

    #[test]
    fn a_cedar_indeterminate_is_insufficient_evidence_not_a_violation() {
        let err = RuntimeError::PolicyDenied {
            reason: "indeterminate: invalid entity uid".to_string(),
        };
        let (public, internal) = tool_auth_denial_classification(&err);
        assert_eq!(public, ErPublicDenialReason::InsufficientEvidence);
        assert_eq!(internal, "policy_indeterminate");
    }

    #[test]
    fn a_typed_policy_denial_stays_a_violation() {
        let err = RuntimeError::PolicyDenied {
            reason: "denied by policy p1".to_string(),
        };
        let (public, internal) = tool_auth_denial_classification(&err);
        assert_eq!(public, ErPublicDenialReason::PolicyDenied);
        assert_eq!(internal, "policy_denied");

        let err = RuntimeError::CapDenied {
            reason: "tool not in allowlist".to_string(),
        };
        let (public, internal) = tool_auth_denial_classification(&err);
        assert_eq!(public, ErPublicDenialReason::PolicyDenied);
        assert_eq!(internal, "tool_not_allowed");

        let (public, internal) = tool_auth_denial_classification(&RuntimeError::CapTokenExpired);
        assert_eq!(public, ErPublicDenialReason::PolicyDenied);
        assert_eq!(internal, "grant_expired");
    }

    #[test]
    fn an_operational_tool_gate_error_is_insufficient_evidence() {
        let err = RuntimeError::Internal(anyhow::anyhow!("store offline"));
        let (public, internal) = tool_auth_denial_classification(&err);
        assert_eq!(public, ErPublicDenialReason::InsufficientEvidence);
        assert_eq!(internal, "tool_invocation_error");
    }

    #[test]
    fn a_mid_turn_revocation_keeps_its_typed_classification() {
        // stage_cap_token_for_tool_at collapses the typed CapTokenError into
        // the CapDenied display string; the classification must re-derive the
        // repository's existing shape rather than flattening every cap
        // failure into tool_not_allowed.
        let err = RuntimeError::CapDenied {
            reason: CapTokenError::Revoked.to_string(),
        };
        let (public, internal) = tool_auth_denial_classification(&err);
        assert_eq!(public, ErPublicDenialReason::Revoked);
        assert_eq!(internal, "revoked");

        let err = RuntimeError::CapDenied {
            reason: CapTokenError::AudienceMismatch.to_string(),
        };
        let (public, internal) = tool_auth_denial_classification(&err);
        assert_eq!(public, ErPublicDenialReason::PolicyDenied);
        assert_eq!(internal, "audience_mismatch");

        let err = RuntimeError::CapDenied {
            reason: CapTokenError::SignatureInvalid.to_string(),
        };
        let (public, internal) = tool_auth_denial_classification(&err);
        assert_eq!(public, ErPublicDenialReason::ChainInvalid);
        assert_eq!(internal, "signature_invalid");

        let err = RuntimeError::CapDenied {
            reason: CapTokenError::BudgetExhausted.to_string(),
        };
        let (public, internal) = tool_auth_denial_classification(&err);
        assert_eq!(public, ErPublicDenialReason::BudgetExhausted);
        assert_eq!(internal, "budget_exhausted");

        // The default stays a policy denial for the remaining shapes.
        let err = RuntimeError::CapDenied {
            reason: CapTokenError::ToolNotAllowed.to_string(),
        };
        let (public, internal) = tool_auth_denial_classification(&err);
        assert_eq!(public, ErPublicDenialReason::PolicyDenied);
        assert_eq!(internal, "tool_not_allowed");
    }

    #[test]
    fn only_an_explicit_human_decision_mints_an_approval_violation() {
        let err = RuntimeError::ApprovalRequired {
            approval_id: "a1".to_string(),
            tool: "t".to_string(),
            reason: "approval-gated capability".to_string(),
        };
        let (public, internal) = approval_denial_classification(&err);
        assert_eq!(public, ErPublicDenialReason::PolicyDenied);
        assert_eq!(internal, "approval_required");

        let err = RuntimeError::ApprovalRejected {
            approval_id: "a1".to_string(),
            tool: "t".to_string(),
            reason: "rejected by operator".to_string(),
        };
        let (public, internal) = approval_denial_classification(&err);
        assert_eq!(public, ErPublicDenialReason::PolicyDenied);
        assert_eq!(internal, "approval_rejected");

        // An approval-store operational failure is NOT a human rejection.
        let err = RuntimeError::Internal(anyhow::anyhow!("approval store offline"));
        let (public, internal) = approval_denial_classification(&err);
        assert_eq!(public, ErPublicDenialReason::InsufficientEvidence);
        assert_eq!(internal, "approval_evaluation_error");
    }

    #[test]
    fn a_memory_policy_evaluation_failure_is_insufficient_evidence() {
        let err = ardur_memory::MemoryError::PolicyIndeterminate {
            action: ardur_memory::MemoryAction::Record,
            reason: "invalid entity uid".to_string(),
        };
        match memory_failure_outcome(&err) {
            EventOutcome::Denied(denied) => {
                assert_eq!(denied.public, ErPublicDenialReason::InsufficientEvidence);
                assert_eq!(denied.internal, "memory_policy_indeterminate");
            }
            other => panic!("expected a denied-as-insufficient outcome, got {other:?}"),
        }

        let err = ardur_memory::MemoryError::PolicyDenied {
            action: ardur_memory::MemoryAction::Record,
            reason: "denied".to_string(),
        };
        match memory_failure_outcome(&err) {
            EventOutcome::Denied(denied) => {
                assert_eq!(denied.public, ErPublicDenialReason::PolicyDenied);
                assert_eq!(denied.internal, "memory_policy_denied");
            }
            other => panic!("expected a typed denial, got {other:?}"),
        }

        let err = ardur_memory::MemoryError::Backend("write failed mid-flight".to_string());
        assert!(
            matches!(memory_failure_outcome(&err), EventOutcome::FailedUnknown),
            "a backend failure leaves the effect unknown"
        );
    }

    #[test]
    fn an_all_read_capability_set_classifies_as_a_read() {
        let classification = classify_tool_invocation("reader", &[Capability::FsRead]);
        assert_eq!(classification.action_class, ErActionClass::Read);
        assert_eq!(classification.resource_family, "filesystem");
        assert_eq!(classification.side_effect_class, ErSideEffectClass::None);

        // The strongest declared effect still wins for mixed sets.
        let classification =
            classify_tool_invocation("mixed", &[Capability::FsRead, Capability::FsWrite]);
        assert_eq!(classification.action_class, ErActionClass::Write);
        assert_eq!(
            classification.side_effect_class,
            ErSideEffectClass::StateChange
        );

        // And the no-capability default is unchanged.
        let classification = classify_tool_invocation("echo", &[]);
        assert_eq!(classification.action_class, ErActionClass::Observe);
        assert_eq!(classification.resource_family, "tool");
    }
}
