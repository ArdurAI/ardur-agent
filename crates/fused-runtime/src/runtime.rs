//! [`FusedRuntime`] — the fused [`ChatRuntime`] and its multi-stage [`submit`]
//! pipeline (cap-token → cedar → cost-gate → pre-submit hooks → injection-defense
//! → provider → receipt → post-receipt hooks → finalize → memory → journal). See
//! the crate root for the full stage list and the Option-B rationale.
//!
//! [`submit`]: FusedRuntime::submit

use std::path::PathBuf;
use std::sync::Arc;

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
use ardur_memory::MemoryRuntime;
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, ModelId, Provider, ProviderError,
};
use ardur_receipt::{Es256SigningKey, ReceiptBody, ReceiptSigner, Sha256Digest, VerbObject};
use ardur_runtime::{
    CapTokenRef, ChatMessage, ChatRuntime, CostTuple as RuntimeCostTuple, ReceiptId, Role,
    RuntimeError, SessionId, SubmitRequest, SubmitResult,
};
use ardur_session_journals::{JournalEntry, SessionJournal};
use parking_lot::Mutex;

use crate::receipts::{PersistedReceipt, load_persisted_chain};
use crate::reconcile::{
    ReconciliationAction, ReconciliationError, ReconciliationReport, ReconciliationStrategy,
};
use crate::shared::{SharedBudget, SharedDenyList};

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
    pub(crate) gate: InMemoryCostAdmissionGate<SharedBudget>,
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
        if req.cap_token.0.is_empty() {
            let err = RuntimeError::CapTokenMissing;
            self.fire_error(session_id, LifecyclePhase::Submit, &err)
                .await;
            return Err(err);
        }
        let token = match CapToken::from_base64(&req.cap_token.0, &self.cap_root) {
            Ok(token) => token,
            Err(e) => {
                let err = RuntimeError::CapDenied {
                    reason: e.to_string(),
                };
                self.fire_error(session_id, LifecyclePhase::Submit, &err)
                    .await;
                return Err(err);
            }
        };
        // The audience the cap-token is verified against: the per-request
        // override if supplied (so one runtime can serve cap-tokens scoped to
        // different tenant audiences), else the builder default.
        let audience = provisioning
            .audience
            .clone()
            .unwrap_or_else(|| self.audience.clone());
        let claims = match self.verifier.verify(
            &token,
            &self.cap_root,
            &RequiredCaveats {
                now_unix,
                audience,
                tool: self.tool.clone(),
                cost: self.cost_units,
            },
        ) {
            Ok(claims) => claims,
            Err(CapTokenError::Expired) => {
                let err = RuntimeError::CapTokenExpired;
                self.fire_error(session_id, LifecyclePhase::Submit, &err)
                    .await;
                return Err(err);
            }
            Err(other) => {
                let err = RuntimeError::CapDenied {
                    reason: other.to_string(),
                };
                self.fire_error(session_id, LifecyclePhase::Submit, &err)
                    .await;
                return Err(err);
            }
        };

        // ---- 2. cedar-policy: authorize the turn. The principal is *derived*
        //         from the verified cap-token subject (stage 1), not asserted by
        //         the caller — so the runtime authorizes as whoever the cap
        //         proved, and a misconfigured caller cannot impersonate another
        //         subject. The resource is the session the turn acts on; the cap
        //         claims (audience, tools, expiry, subject, budget) ride as
        //         resource attributes so policies can reference `resource.<key>`.
        let principal = derive_principal(&self.principal_entity_type, &claims);
        let resource = derive_resource(session_id);
        let attributes = cedar_attributes_from_claims(&self.cedar_attributes, &claims);
        match self.policies.evaluate(&EvaluationContext {
            principal,
            action: self.action.clone(),
            resource,
            attributes,
        }) {
            Decision::Allow { .. } => {}
            Decision::Deny { reason, .. } => {
                let err = RuntimeError::PolicyDenied { reason };
                self.fire_error(session_id, LifecyclePhase::Submit, &err)
                    .await;
                return Err(err);
            }
            Decision::Indeterminate { reason } => {
                let err = RuntimeError::PolicyDenied {
                    reason: format!("indeterminate: {reason}"),
                };
                self.fire_error(session_id, LifecyclePhase::Submit, &err)
                    .await;
                return Err(err);
            }
        }

        // ---- 3. cost-gate: resolve the budget holder, optionally top it up for
        //         this request, bind the verified token to it, then reserve the
        //         projected envelope.
        //
        // The holder is the verified cap-token subject (so a turn spends against
        // whoever the cap proved), unless the caller overrides it — rare, and
        // reserved for impersonation-test fixtures. If the request carries a
        // budget, provision it onto the holder *before* admission so a freshly
        // funded subject can reserve against the new balance; the merge is
        // additive, so a per-turn top-up accumulates rather than zeroing unspent
        // budget.
        let gate_token_id = GateTokenId(claims.token_id);
        let holder = provisioning
            .subject
            .clone()
            .unwrap_or_else(|| GateHolderId(claims.subject.0.clone()));
        if let Some(budget) = provisioning.budget {
            if let Err(e) = self.gate.provision_for(&holder, budget).await {
                let err = RuntimeError::ProvisioningFailed {
                    subject: holder.0.clone(),
                    reason: e.to_string(),
                };
                self.fire_error(session_id, LifecyclePhase::Submit, &err)
                    .await;
                return Err(err);
            }
        }
        self.gate.bind_token(gate_token_id, holder.clone());
        let request_digest = GateSha256::of(&serde_json::to_vec(&req.messages).unwrap_or_default());
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

        // ---- 4. pre-submit hooks. A veto aborts (releasing the reservation); a
        //         replace swaps the request used from here on.
        let base_request =
            CompletionRequest::new(req.messages.clone(), self.model.clone(), self.max_tokens);
        let pre_ctx = PreSubmitCtx {
            session_id,
            request: &base_request,
            cap_token_id: &req.cap_token,
            attempt: 1,
        };
        let request = match self.registry.run_pre_submit(&pre_ctx).await {
            PreSubmitOutcome::Continue => base_request,
            PreSubmitOutcome::Replaced { request } => request,
            PreSubmitOutcome::Vetoed { hook_id, reason } => {
                self.release(reservation).await;
                return Err(RuntimeError::VetoedByHook {
                    hook_id: hook_id.to_string(),
                    reason,
                });
            }
        };

        // ---- 4.5 injection-defense: scan the (possibly hook-rewritten) outbound
        //          prompt before it reaches the provider. `Allow` forwards it
        //          unchanged; `AllowWithSanitization` swaps the provider body for
        //          the redacted rewrite (the *raw* prompt still rides through to
        //          the journal at stage 10, which reads `req.messages`, not this
        //          local request); `Block` releases the reservation and returns
        //          `InjectionBlocked` — so the provider, and every billing /
        //          receipt side effect downstream of it, never runs. An empty
        //          filter registry (the default) is a no-op short-circuit.
        //
        // TODO ARD-22: tool outputs through filter as ToolReturn when tool-use lands.
        let request = match self.scan_outbound_request(request).await {
            Ok(request) => request,
            Err(err) => {
                self.release(reservation).await;
                self.fire_error(session_id, LifecyclePhase::Submit, &err)
                    .await;
                return Err(err);
            }
        };

        // ---- 5. provider: real dispatch.
        let response = match self.provider.complete(request).await {
            Ok(response) => response,
            Err(provider_err) => {
                self.release(reservation).await;
                self.fire_error(session_id, LifecyclePhase::Provider, &provider_err)
                    .await;
                return Err(map_provider_error(&provider_err));
            }
        };

        // ---- 6. receipt: mint, chain onto the prior receipt, sign, persist.
        let parent_hash = *self.chain_tail.lock();
        let body = ReceiptBody {
            receipt_id: uuid::Uuid::new_v4(),
            parent_hash,
            verb: self.verb.clone(),
            issued_at: ardur_receipt::UnixTsMillis(now_ms),
            subject: ardur_receipt::HolderId(claims.subject.0.clone()),
            cap_token_id: ardur_receipt::TokenId(claims.token_id.to_string()),
            payload_digest: Sha256Digest::of(response.content.as_bytes()),
            cost: runtime_cost_to_receipt(&response.cost),
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
        // Advance the chain tail to this receipt's JWS hash and persist the line
        // before observers run, so the chain state is consistent the moment a
        // post-receipt hook (or a crash) sees the receipt. We set `parent_hash`
        // from the stored digest rather than via `ReceiptChain::append` because
        // a restart resumes from a hash, not a reconstructable `SignedReceipt`.
        *self.chain_tail.lock() = Some(Sha256Digest::of(signed.jws_compact().as_bytes()));
        if let Err(e) = self.persist_receipt(signed.jws_compact()) {
            self.fire_error(session_id, LifecyclePhase::Receipt, &e)
                .await;
        }
        let receipt = signed.body().clone();

        // ---- 7. post-receipt hooks (observational; the turn already happened).
        let post_ctx = PostReceiptCtx {
            session_id,
            receipt: &receipt,
            response: &response,
            cost: response.cost,
        };
        for err in self.registry.run_post_receipt(&post_ctx).await {
            let _ = err;
        }

        // ---- 8. cost-gate finalize: post the actual cost, refund the unspent
        //         delta. A settlement failure auto-releases the hold inside the
        //         gate, so we never strand the budget and never fail the
        //         already-receipted turn over it.
        let actual = runtime_cost_to_gate(&response.cost);
        let _ = self.gate.finalize(reservation, actual).await;

        // ---- 9. memory: record the turn as a bi-temporal fact. Non-fatal.
        if let Some(memory) = &self.memory {
            let record = turn_record(&claims.subject.0, &response, &receipt, now_ms);
            if let Err(mem_err) = memory.record(record) {
                self.fire_error(session_id, LifecyclePhase::MemoryWrite, &mem_err)
                    .await;
            }
        }

        // ---- 10. session-journal: append the user + assistant messages. The
        //          backend fsyncs each entry. Non-fatal.
        if let Some(journal) = &self.journal {
            if let Some(prompt) = last_user_message(&req.messages) {
                if let Err(e) = journal
                    .append(JournalEntry::UserMessage {
                        content: prompt.to_string(),
                        at: now_ms,
                    })
                    .await
                {
                    self.fire_error(session_id, LifecyclePhase::JournalAppend, &e)
                        .await;
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
                self.fire_error(session_id, LifecyclePhase::JournalAppend, &e)
                    .await;
            }
        }

        Ok(SubmitResult {
            receipt_id: ReceiptId(receipt.receipt_id),
            response: ChatMessage::assistant(response.content),
            cost: response.cost,
        })
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
        }),
        now,
        now,
        None,
        now,
    );
    record.source_receipt_id = Some(ardur_memory::ReceiptId(receipt.receipt_id));
    record
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
