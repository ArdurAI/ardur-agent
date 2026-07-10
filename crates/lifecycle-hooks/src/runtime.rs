//! [`HookedRuntime`] — a [`ChatRuntime`] that threads a [`HookRegistry`] through
//! the turn lifecycle.
//!
//! Pipeline of one [`ChatRuntime::submit`]:
//!
//! 1. Admit the request (Phase 1: reject an empty cap-token). On failure, fire
//!    `on_error` with [`LifecyclePhase::Submit`] and return.
//! 2. Build the [`CompletionRequest`] and run **pre-submit** hooks. A veto
//!    returns [`RuntimeError::VetoedByHook`] *without* calling the provider; a
//!    replace swaps the request used from here on.
//! 3. Call the provider. On failure, fire `on_error` with
//!    [`LifecyclePhase::Provider`] and return.
//! 4. Mint the receipt, then run **post-receipt** hooks (their errors are
//!    logged, never fatal — the turn already happened).
//! 5. Write the turn record to memory (if configured). A memory failure fires
//!    `on_error` with [`LifecyclePhase::MemoryWrite`] but does not fail the turn.
//!
//! The cap-token revoke path is [`HookedRuntime::revoke_cap_token`], which fires
//! `on_revoke` across the registry.

use std::sync::Arc;

use ardur_memory::MemoryRuntime;
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, ModelId, Provider, ProviderError,
};
use ardur_receipt::{Es256SigningKey, ReceiptBody, ReceiptSigner, Sha256Digest, VerbObject};
use ardur_runtime::{
    CapTokenRef, ChatMessage, ChatRuntime, ReceiptId, RuntimeError, SessionId, SubmitRequest,
    SubmitResult,
};

use crate::hook::{ErrorCtx, LifecyclePhase, PostReceiptCtx, PreSubmitCtx, RevokeCtx};
use crate::registry::{HookRegistry, PreSubmitOutcome};

/// The receipt verb minted for a completed turn (`verb.object.state.vN`).
const COMPLETION_VERB: &str = "llm.completion.minted.v1";

/// Default output-token ceiling when building a [`CompletionRequest`] from a
/// [`SubmitRequest`] (which carries no model parameters in Phase 1).
const DEFAULT_MAX_TOKENS: u32 = 1024;

/// A [`ChatRuntime`] that runs a [`HookRegistry`] across the turn lifecycle,
/// dispatching the completion through a [`Provider`] and (optionally) writing
/// the turn to a [`MemoryRuntime`].
#[derive(Clone)]
pub struct HookedRuntime {
    registry: Arc<HookRegistry>,
    provider: Arc<dyn Provider>,
    model: ModelId,
    max_tokens: u32,
    receipt_key: Es256SigningKey,
    memory: Option<Arc<dyn MemoryRuntime + Send + Sync>>,
}

impl HookedRuntime {
    /// Build a hooked runtime dispatching to `provider` under `model`, with the
    /// given hook registry. No memory sink and the default token ceiling; add a
    /// memory sink with [`with_memory`](Self::with_memory).
    pub fn new(registry: Arc<HookRegistry>, provider: Arc<dyn Provider>, model: ModelId) -> Self {
        Self {
            registry,
            provider,
            model,
            max_tokens: DEFAULT_MAX_TOKENS,
            receipt_key: Es256SigningKey::generate(),
            memory: None,
        }
    }

    /// Attach a memory sink the completed turn is recorded into (after
    /// `on_post_receipt`).
    #[must_use]
    pub fn with_memory(mut self, memory: Arc<dyn MemoryRuntime + Send + Sync>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Override the output-token ceiling used when building the provider
    /// request.
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Use explicit ES256 key material for receipts minted by this runtime.
    /// Without this, the runtime generates an ephemeral key when it is built.
    #[must_use]
    pub fn with_receipt_key(mut self, receipt_key: Es256SigningKey) -> Self {
        self.receipt_key = receipt_key;
        self
    }

    /// The hook registry this runtime threads through every turn.
    #[must_use]
    pub fn registry(&self) -> &Arc<HookRegistry> {
        &self.registry
    }

    /// Fire `on_revoke` across the registry for a cap-token revoked mid-session.
    /// Returns any errors hooks raised (collected, not fatal). This is the
    /// runtime's cap-token revoke entry point.
    pub async fn revoke_cap_token(
        &self,
        session_id: SessionId,
        cap_token_id: CapTokenRef,
        revocation_reason: impl Into<String>,
    ) -> Vec<crate::hook::HookError> {
        let ctx = RevokeCtx {
            session_id,
            cap_token_id: &cap_token_id,
            revocation_reason: revocation_reason.into(),
        };
        self.registry.run_revoke(&ctx).await
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
}

impl ChatRuntime for HookedRuntime {
    async fn submit(&self, req: SubmitRequest) -> Result<SubmitResult, RuntimeError> {
        let session_id = req.session_id;

        // 1. Admission. Phase 1 admits any non-empty cap-token; the cost-gate
        //    wiring lands with §11.14's gate (this is the seam the gate slots
        //    into, before pre-submit).
        if req.cap_token.0.is_empty() {
            let err = RuntimeError::CapTokenMissing;
            self.fire_error(session_id, LifecyclePhase::Submit, &err)
                .await;
            return Err(err);
        }

        // 2. Build the provider request and run pre-submit hooks.
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
                // A veto is a hook *decision*, not a provider/cost-gate error,
                // so it surfaces as its own error variant and does not fire
                // `on_error`. The provider is never called.
                return Err(RuntimeError::VetoedByHook {
                    hook_id: hook_id.to_string(),
                    reason,
                });
            }
        };

        // 3. Provider call.
        let response = match self.provider.complete(request).await {
            Ok(resp) => resp,
            Err(provider_err) => {
                self.fire_error(session_id, LifecyclePhase::Provider, &provider_err)
                    .await;
                return Err(map_provider_error(&provider_err));
            }
        };

        // 4. Mint + sign the receipt, then run post-receipt hooks
        //    (observational).
        let body = mint_receipt(&req, &response);
        let signed_receipt = match ReceiptSigner::sign(body, &self.receipt_key) {
            Ok(signed) => signed,
            Err(receipt_err) => {
                self.fire_error(session_id, LifecyclePhase::Receipt, &receipt_err)
                    .await;
                return Err(RuntimeError::Internal(anyhow::anyhow!(
                    "receipt mint failed: {receipt_err}"
                )));
            }
        };
        let receipt = signed_receipt.body();
        let post_ctx = PostReceiptCtx {
            session_id,
            signed_receipt: &signed_receipt,
            receipt,
            response: &response,
            cost: response.cost,
        };
        for err in self.registry.run_post_receipt(&post_ctx).await {
            // Observational: log and continue — non-fatal contract.
            tracing::warn!(error = %err, "post-receipt hook error (non-fatal)");
        }

        // 5. Memory write (after the post-receipt observers have seen the
        //    receipt). A write failure is reported via `on_error` but does not
        //    fail the turn — the turn already happened and is receipted.
        if let Some(memory) = &self.memory {
            let record = turn_record(&req, &response, receipt);
            if let Err(mem_err) = memory.record(record) {
                self.fire_error(session_id, LifecyclePhase::MemoryWrite, &mem_err)
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

/// Map a provider failure onto the runtime's error surface.
fn map_provider_error(err: &ProviderError) -> RuntimeError {
    match err {
        ProviderError::CostCeilingExceeded => RuntimeError::CostCeilingExceeded,
        _ => RuntimeError::ProviderUnavailable,
    }
}

/// Mint the receipt body for a completed turn. The `payload_digest` covers the
/// *response actually produced*, so a turn whose request was redacted by a
/// pre-submit hook is receipted against the redacted text.
fn mint_receipt(req: &SubmitRequest, response: &CompletionResponse) -> ReceiptBody {
    ReceiptBody {
        receipt_id: uuid::Uuid::new_v4(),
        parent_hash: None,
        verb: VerbObject::new(COMPLETION_VERB).expect("COMPLETION_VERB is a valid receipt verb"),
        issued_at: ardur_receipt::UnixTsMillis(now_millis()),
        subject: ardur_receipt::HolderId(req.session_id.0.to_string()),
        cap_token_id: ardur_receipt::TokenId(req.cap_token.0.clone()),
        payload_digest: Sha256Digest::of(response.content.as_bytes()),
        session_id: Some(req.session_id.0),
        cost: to_receipt_cost(response.cost),
        // This Phase-1 hooked runtime does not execute tools; the §6.0
        // tool-call receipts are minted by the fused runtime.
        tool_calls: Vec::new(),
        // The §11.14b provider field is populated by the fused-runtime mint,
        // which holds the `dyn Provider`; this hooked runtime leaves it unset.
        provider: None,
    }
}

/// Build the bi-temporal memory record for a completed turn.
fn turn_record(
    req: &SubmitRequest,
    response: &CompletionResponse,
    receipt: &ReceiptBody,
) -> ardur_memory::MemoryRecord {
    let now = ardur_memory::UnixTsMillis(now_millis());
    let mut record = ardur_memory::MemoryRecord::new(
        ardur_memory::HolderId(req.session_id.0.to_string()),
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

/// Convert the runtime's `CostTuple` into the receipt crate's identically-shaped
/// `CostTuple` (the two are reconciled into one type in a later §0.0 amendment).
fn to_receipt_cost(cost: ardur_runtime::CostTuple) -> ardur_receipt::CostTuple {
    ardur_receipt::CostTuple {
        tokens_in: cost.tokens_in,
        tokens_out: cost.tokens_out,
        cents: cost.cents,
        wall_ms: cost.wall_ms,
        attention_score: cost.attention_score,
    }
}

/// Wall-clock milliseconds since the Unix epoch (saturating to `0` before the
/// epoch — unreachable in practice).
fn now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
