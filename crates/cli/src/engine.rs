//! [`ChatEngine`] — the substrate a `ardur chat` session drives.
//!
//! It wires the §2.1 blueprint's Phase-1 stack: the §1.0 [`InMemoryRuntime`],
//! the §11.14 cost gate ([`InMemoryCostAdmissionGate`] over an
//! [`InMemoryBudgetStore`]), and the §3.0 [`ProviderRegistry`] holding the
//! Anthropic stub. [`ChatEngine::run_turn`] is the per-message path: resolve the
//! provider, admit the turn against the budget, submit it to the runtime, and
//! finalize the actual cost.
//!
//! The displayed budget is a shared [`AtomicU64`] of remaining cents, seeded
//! from the provisioned balance and decremented by each turn's actual cost. It
//! is the single value the prompt indicator and the `/budget` command read.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ardur_cost_gate::{
    AdmissionRequest, CostAdmissionGate, CostEnvelope, CostTuple as GateCostTuple, HolderId,
    InMemoryBudgetStore, InMemoryCostAdmissionGate, ModelId as GateModelId,
    ProviderId as GateProviderId, Sha256Digest, TokenId,
};
use ardur_provider_runtime::{AnthropicProvider, ModelId, Provider, ProviderRegistry};
use ardur_runtime::{
    CapTokenRef, ChatMessage, ChatRuntime, InMemoryRuntime, ProviderId, RuntimeError, SessionId,
    SubmitRequest,
};
use uuid::Uuid;

use crate::config::Config;
use crate::error::CliError;

/// The holder the Phase-1 session spends its budget against. Phase 2 resolves
/// this from the verified cap-token's claims.
const SESSION_HOLDER: &str = "cli-session";

/// The placeholder capability token authorizing the session's turns. The
/// runtime only requires it to be non-empty in Phase 1.
// TODO §2.1 Phase 2: mint a real Biscuit via `ardur-cap-token` and attenuate it
// per session instead of this opaque placeholder.
const SESSION_CAP_TOKEN: &str = "cli-dev-cap-token";

/// The outcome of one chat turn: the assistant's reply and the turn's cost
/// accounting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnOutcome {
    /// The assistant's response text.
    pub response: String,
    /// Cents this turn actually cost.
    pub used_cents: u64,
    /// Cents remaining in the session budget after this turn.
    pub remaining_cents: u64,
}

/// The wired chat substrate for one interactive session.
pub struct ChatEngine {
    runtime: InMemoryRuntime,
    gate: InMemoryCostAdmissionGate<InMemoryBudgetStore>,
    registry: ProviderRegistry,
    provider_id: ProviderId,
    model: ModelId,
    cap_token: CapTokenRef,
    cap_token_id: TokenId,
    session_id: SessionId,
    remaining: Arc<AtomicU64>,
}

impl ChatEngine {
    /// Wire a fresh engine from `config`: provision the budget, bind the
    /// session's cap-token to its holder, and register the Anthropic provider.
    pub fn new(config: &Config) -> Result<Self, CliError> {
        let holder = HolderId(SESSION_HOLDER.to_string());

        // Provision the budget store with the configured starting balance, then
        // hand it to the gate (which owns it for the session's lifetime).
        let budget = InMemoryBudgetStore::new();
        budget.set_balance(holder.clone(), GateCostTuple::cents(config.budget_cents));

        let cap_token_id = TokenId(Uuid::new_v4());
        let gate = InMemoryCostAdmissionGate::new(budget);
        gate.bind_token(cap_token_id, holder);

        let mut registry = ProviderRegistry::new();
        let provider = AnthropicProvider::new(config.api_key.clone(), ModelId::new(&config.model));
        let provider_id = provider.id();
        registry.register(Arc::new(provider));

        Ok(Self {
            runtime: InMemoryRuntime::new(),
            gate,
            registry,
            provider_id,
            model: ModelId::new(&config.model),
            cap_token: CapTokenRef(SESSION_CAP_TOKEN.to_string()),
            cap_token_id,
            session_id: SessionId::new(),
            remaining: Arc::new(AtomicU64::new(config.budget_cents)),
        })
    }

    /// A shared handle to the session's remaining-cents counter — read by the
    /// prompt indicator and the `/budget` command.
    #[must_use]
    pub fn budget_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.remaining)
    }

    /// The session's remaining budget, in cents.
    #[must_use]
    pub fn remaining_cents(&self) -> u64 {
        self.remaining.load(Ordering::SeqCst)
    }

    /// Run one chat turn over `messages` (oldest first; the last user message is
    /// the prompt). Resolves the provider, admits the turn against the budget,
    /// submits it to the runtime, finalizes the actual cost, and returns the
    /// reply plus the cost accounting.
    pub async fn run_turn(&self, messages: &[ChatMessage]) -> Result<TurnOutcome, CliError> {
        // The runtime echoes rather than dispatching to the provider in Phase 1,
        // but the requested provider must still be registered — resolve it up
        // front so an unknown provider fails before any budget is reserved.
        if self.registry.get(&self.provider_id).is_none() {
            return Err(CliError::Runtime(RuntimeError::ProviderUnavailable));
        }

        // Stages 1–3: admit the turn. Phase 1 projects a zero envelope (the stub
        // bills nothing); Phase 2 projects real usage from the prompt.
        // TODO §2.1 Phase 2: project the envelope from a token estimate and
        // enforce a per-call ceiling here.
        let request_digest = Sha256Digest::of(&serde_json::to_vec(messages).unwrap_or_default());
        let reservation = self
            .gate
            .admit(AdmissionRequest {
                cap_token_id: self.cap_token_id,
                projected_envelope: CostEnvelope::default(),
                provider_id: GateProviderId(self.provider_id.0.clone()),
                model_id: GateModelId(self.model.0.clone()),
                request_digest,
            })
            .await?;

        // Submit the turn to the runtime.
        let result = self
            .runtime
            .submit(SubmitRequest {
                messages: messages.to_vec(),
                cap_token: self.cap_token.clone(),
                session_id: self.session_id,
                requested_provider: Some(self.provider_id.clone()),
            })
            .await?;

        // Stage 4: finalize the actual cost and refund the unspent delta.
        let used_cents = result.cost.cents;
        self.gate.finalize(reservation, result.cost).await?;

        // Decrement the displayed budget by the turn's actual cost, saturating
        // at zero.
        let remaining = self
            .remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |bal| {
                Some(bal.saturating_sub(used_cents))
            })
            // `fetch_update` only returns `Err` when the closure returns `None`,
            // which it never does — so the post-decrement balance is `Ok`.
            .map(|prev| prev.saturating_sub(used_cents))
            .unwrap_or(0);

        Ok(TurnOutcome {
            response: result.response.content,
            used_cents,
            remaining_cents: remaining,
        })
    }
}
