//! [`FusedRuntimeBuilder`] — assembles a [`FusedRuntime`] from the substrate
//! pieces, applying sensible Phase-2 defaults so a caller specifies only what it
//! cares about.
//!
//! The four pieces with no reasonable default are required up front
//! ([`new`](FusedRuntimeBuilder::new)): the cap-token root key, the policy
//! bundle, the provider, the receipt signing key, and the model. Everything else
//! — audience, tool, the Cedar principal/action/resource, the budget, the
//! projected envelope, the hook registry, memory, journal, and the receipt log —
//! has a default and an opt-in setter.

use std::path::PathBuf;
use std::sync::Arc;

use ardur_cedar_policy::{ActionRef, CedarPolicyBundle, PrincipalRef, ResourceRef};
use ardur_cost_gate::{
    Clock, CostEnvelope, CostTuple as GateCostTuple, HolderId as GateHolderId,
    InMemoryCostAdmissionGate, SystemClock,
};
use ardur_lifecycle_hooks::HookRegistry;
use ardur_memory::MemoryRuntime;
use ardur_provider_runtime::{ModelId, Provider};
use ardur_receipt::{Es256SigningKey, VerbObject};
use ardur_session_journals::SessionJournal;
use parking_lot::Mutex;

use crate::receipts::{ReceiptChainError, load_persisted_chain};
use crate::runtime::{COMPLETION_VERB, FusedRuntime};
use crate::shared::{SharedBudget, SharedDenyList};

/// A generous default per-turn envelope: enough that the cost gate is exercised
/// (it reserves and finalizes) without rejecting a normal turn. Tests that want
/// to exhaust the budget set a tighter envelope via
/// [`projected_envelope`](FusedRuntimeBuilder::projected_envelope).
const DEFAULT_ENVELOPE: CostEnvelope = CostEnvelope {
    tokens_in_max: 100_000,
    tokens_out_max: 100_000,
    cents_max: 1_000_000,
    wall_ms_max: 600_000,
    attention_score_max: 1_000_000,
};

/// Builder for [`FusedRuntime`]. See the module docs for the default policy.
pub struct FusedRuntimeBuilder {
    cap_root: ardur_cap_token::PublicKey,
    policies: CedarPolicyBundle,
    provider: Arc<dyn Provider>,
    receipt_key: Es256SigningKey,
    model: ModelId,

    audience: String,
    tool: String,
    cost_units: u64,
    clock: Arc<dyn Clock>,
    principal: PrincipalRef,
    action: ActionRef,
    resource: ResourceRef,
    cedar_attributes: serde_json::Value,
    max_tokens: u32,
    verb: VerbObject,
    budget: SharedBudget,
    deny: SharedDenyList,
    envelope: CostEnvelope,
    ceiling: Option<CostEnvelope>,
    registry: Arc<HookRegistry>,
    memory: Option<Arc<dyn MemoryRuntime + Send + Sync>>,
    journal: Option<Arc<dyn SessionJournal>>,
    receipt_log: Option<PathBuf>,
}

impl FusedRuntimeBuilder {
    /// Start a builder. The cap-token root key, the Cedar bundle, the provider,
    /// the receipt signing key, and the model are required; the rest default.
    #[must_use]
    pub fn new(
        cap_root: ardur_cap_token::PublicKey,
        policies: CedarPolicyBundle,
        provider: Arc<dyn Provider>,
        receipt_key: Es256SigningKey,
        model: ModelId,
    ) -> Self {
        Self {
            cap_root,
            policies,
            provider,
            receipt_key,
            model,
            audience: "ardur".to_string(),
            tool: "chat.submit".to_string(),
            cost_units: 1,
            clock: Arc::new(SystemClock),
            principal: PrincipalRef("User::agent".to_string()),
            action: ActionRef("Action::Submit".to_string()),
            resource: ResourceRef("Turn::current".to_string()),
            cedar_attributes: serde_json::Value::Null,
            max_tokens: 1024,
            verb: VerbObject::new(COMPLETION_VERB)
                .expect("COMPLETION_VERB is a valid receipt verb"),
            budget: SharedBudget::new(),
            deny: SharedDenyList::new(),
            envelope: DEFAULT_ENVELOPE,
            ceiling: None,
            registry: Arc::new(HookRegistry::new()),
            memory: None,
            journal: None,
            receipt_log: None,
        }
    }

    /// The audience the cap-token must be scoped to (verifier caveat).
    #[must_use]
    pub fn audience(mut self, audience: impl Into<String>) -> Self {
        self.audience = audience.into();
        self
    }

    /// The tool/capability name the turn exercises (verifier caveat).
    #[must_use]
    pub fn tool(mut self, tool: impl Into<String>) -> Self {
        self.tool = tool.into();
        self
    }

    /// The budget units this turn claims against the cap-token (verifier caveat).
    #[must_use]
    pub fn cost_units(mut self, cost_units: u64) -> Self {
        self.cost_units = cost_units;
        self
    }

    /// The clock backing both the cap-token `now` caveat (seconds) and the cost
    /// gate's reservation expiry (milliseconds). Use a `ManualClock` for
    /// deterministic tests.
    #[must_use]
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// The Cedar principal / action / resource the turn is authorized as.
    #[must_use]
    pub fn cedar(
        mut self,
        principal: PrincipalRef,
        action: ActionRef,
        resource: ResourceRef,
    ) -> Self {
        self.principal = principal;
        self.action = action;
        self.resource = resource;
        self
    }

    /// The Cedar resource attributes (the JSON object read as `resource.<key>`).
    #[must_use]
    pub fn cedar_attributes(mut self, attributes: serde_json::Value) -> Self {
        self.cedar_attributes = attributes;
        self
    }

    /// The output-token ceiling on the provider request.
    #[must_use]
    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// The verb minted on each turn's receipt.
    #[must_use]
    pub fn verb(mut self, verb: VerbObject) -> Self {
        self.verb = verb;
        self
    }

    /// Provision (or overwrite) a holder's budget balance.
    #[must_use]
    pub fn provision_budget(self, holder: GateHolderId, balance: GateCostTuple) -> Self {
        self.budget.set_balance(holder, balance);
        self
    }

    /// The per-turn envelope the cost gate reserves against the budget.
    #[must_use]
    pub fn projected_envelope(mut self, envelope: CostEnvelope) -> Self {
        self.envelope = envelope;
        self
    }

    /// Impose a hard per-call ceiling on the cost gate.
    #[must_use]
    pub fn ceiling(mut self, ceiling: CostEnvelope) -> Self {
        self.ceiling = Some(ceiling);
        self
    }

    /// Replace the (empty) hook registry.
    #[must_use]
    pub fn registry(mut self, registry: Arc<HookRegistry>) -> Self {
        self.registry = registry;
        self
    }

    /// Share an externally-held deny-list (so the caller can revoke through its
    /// own handle too). By default the runtime owns a fresh one, reachable via
    /// [`FusedRuntime::revoke_cap_token`].
    #[must_use]
    pub fn deny_list(mut self, deny: SharedDenyList) -> Self {
        self.deny = deny;
        self
    }

    /// Attach the bi-temporal memory sink (stage 9).
    #[must_use]
    pub fn with_memory(mut self, memory: Arc<dyn MemoryRuntime + Send + Sync>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Attach the durable session journal (stage 10).
    #[must_use]
    pub fn with_journal(mut self, journal: Arc<dyn SessionJournal>) -> Self {
        self.journal = Some(journal);
        self
    }

    /// Persist signed receipts to this append-only log (one compact JWS per
    /// line). A fresh runtime over the same path resumes the chain from the last
    /// line — the seam E2E #4 exercises across a restart.
    #[must_use]
    pub fn receipt_log(mut self, path: impl Into<PathBuf>) -> Self {
        self.receipt_log = Some(path.into());
        self
    }

    /// Assemble the runtime. Fails only if a configured [`receipt_log`] exists
    /// but cannot be read back to resume the chain.
    ///
    /// [`receipt_log`]: FusedRuntimeBuilder::receipt_log
    pub fn build(self) -> Result<FusedRuntime, ReceiptChainError> {
        // Seed the chain tail from the persisted log, if any, so a restart
        // continues the chain rather than starting a fresh genesis.
        let chain_tail = match &self.receipt_log {
            Some(path) => load_persisted_chain(path)?
                .last()
                .map(|r| ardur_receipt::Sha256Digest::of(r.jws_compact.as_bytes())),
            None => None,
        };

        let verifier = ardur_cap_token::BiscuitCapTokenVerifier::new(self.deny.clone());

        let mut gate =
            InMemoryCostAdmissionGate::with_clock(self.budget.clone(), self.clock.clone());
        if let Some(ceiling) = self.ceiling {
            gate = gate.with_ceiling(ceiling);
        }

        let gate_provider_id = ardur_cost_gate::ProviderId(self.provider.id().0);
        let gate_model_id = ardur_cost_gate::ModelId(self.model.0.clone());

        Ok(FusedRuntime {
            cap_root: self.cap_root,
            verifier,
            deny: self.deny,
            audience: self.audience,
            tool: self.tool,
            cost_units: self.cost_units,
            clock: self.clock,
            policies: self.policies,
            principal: self.principal,
            action: self.action,
            resource: self.resource,
            cedar_attributes: self.cedar_attributes,
            provider: self.provider,
            model: self.model,
            max_tokens: self.max_tokens,
            receipt_key: self.receipt_key,
            verb: self.verb,
            gate,
            budget: self.budget,
            gate_provider_id,
            gate_model_id,
            envelope: self.envelope,
            registry: self.registry,
            memory: self.memory,
            journal: self.journal,
            chain_tail: Mutex::new(chain_tail),
            receipt_log: self.receipt_log,
        })
    }
}
