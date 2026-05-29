//! The [`MultiAgentRuntime`] trait and its Phase-1 in-memory implementation.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use parking_lot::RwLock;

use ardur_cap_token::{BiscuitCapTokenAttenuator, CapToken, CapTokenAttenuator, Caveat, PublicKey};
use ardur_receipt::{CostTuple, UnixTsMillis};
use ardur_runtime::{
    CapTokenRef, ChatRuntime, CostTuple as RuntimeCostTuple, InMemoryRuntime, ReceiptId, SessionId,
    SubmitRequest,
};

use crate::agent::SubAgent;
use crate::error::MultiAgentError;
use crate::types::{
    AgentId, SubAgentHandle, SubAgentRequest, SubAgentResponse, SubAgentSpec, TerminationReason,
    TerminationReceipt,
};

/// Wall-clock now, in milliseconds since the Unix epoch, in the §11.14 receipt
/// timestamp vocab.
fn now_millis() -> UnixTsMillis {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    UnixTsMillis(u64::try_from(ms).unwrap_or(u64::MAX))
}

/// Project a child turn's [`RuntimeCostTuple`] into the receipt-vocab
/// [`CostTuple`], stamping the cents actually reserved against the envelope.
fn receipt_cost(child: RuntimeCostTuple, charged_cents: u32) -> CostTuple {
    CostTuple {
        tokens_in: child.tokens_in,
        tokens_out: child.tokens_out,
        // The child echo runtime charges zero; the envelope is debited the
        // caller-declared reservation, so that is what the receipt records.
        cents: u64::from(charged_cents),
        wall_ms: child.wall_ms,
        attention_score: child.attention_score,
    }
}

/// Spawn, drive, and tear down isolated child agents.
///
/// Each spawned sub-agent wraps a child [`ChatRuntime`] under a cap-token
/// narrowed from the parent's authority, runs its turns in an isolated session,
/// debits a per-agent budget envelope, and emits a [`TerminationReceipt`] that
/// links into the parent's audit chain on shutdown.
///
/// The trait is object-safe (`dyn MultiAgentRuntime` is usable) but `?Send`:
/// the §1.0 `ChatRuntime::submit` future carries no Send bound, so neither can
/// the futures that await it.
#[async_trait(?Send)]
pub trait MultiAgentRuntime {
    /// Spawn a sub-agent from `spec`: derive its narrowed cap-token, allocate
    /// its budget envelope, register its isolated session, and return a handle.
    async fn spawn(&self, spec: SubAgentSpec) -> Result<SubAgentHandle, MultiAgentError>;

    /// Send one chat turn to a sub-agent. The turn's `max_cost_cents` is
    /// reserved against the sub-agent's envelope before the child runs; an ask
    /// that would exceed the envelope returns [`MultiAgentError::BudgetExhausted`]
    /// without invoking the model.
    async fn ask(
        &self,
        handle: &SubAgentHandle,
        request: SubAgentRequest,
    ) -> Result<SubAgentResponse, MultiAgentError>;

    /// Cleanly shut down a sub-agent, emitting a [`TerminationReceipt`] for the
    /// parent's audit chain. The handle is consumed; a second termination of the
    /// same id returns [`MultiAgentError::AlreadyTerminated`].
    async fn terminate(
        &self,
        handle: SubAgentHandle,
        reason: TerminationReason,
    ) -> Result<TerminationReceipt, MultiAgentError>;

    /// The handles of every currently-live sub-agent (terminated ones are
    /// excluded).
    fn list(&self) -> Vec<SubAgentHandle>;
}

/// The Phase-1 in-memory [`MultiAgentRuntime`]: a `parking_lot`-guarded registry
/// of [`SubAgent`]s, each wrapping a shared child runtime.
///
/// The runtime carries the parent agent's authority (its cap-token + the issuer
/// root key it verifies against) and a `parent_receipt_id` anchor; every spawned
/// sub-agent narrows that token and links its termination receipt to that
/// anchor.
pub struct InMemoryMultiAgentRuntime<R: ChatRuntime = InMemoryRuntime> {
    child_runtime: Arc<R>,
    parent_cap_token: CapToken,
    root: PublicKey,
    parent_receipt_id: ReceiptId,
    attenuator: BiscuitCapTokenAttenuator,
    agents: RwLock<HashMap<AgentId, SubAgent<R>>>,
    terminated: RwLock<HashSet<AgentId>>,
}

impl<R: ChatRuntime> InMemoryMultiAgentRuntime<R> {
    /// Build a runtime over a child runtime, the parent's cap-token, the issuer
    /// root key the parent token verifies against, and the parent receipt anchor
    /// that sub-agent termination receipts link back to.
    pub fn new(
        child_runtime: R,
        parent_cap_token: CapToken,
        root: PublicKey,
        parent_receipt_id: ReceiptId,
    ) -> Self {
        Self {
            child_runtime: Arc::new(child_runtime),
            parent_cap_token,
            root,
            parent_receipt_id,
            attenuator: BiscuitCapTokenAttenuator,
            agents: RwLock::new(HashMap::new()),
            terminated: RwLock::new(HashSet::new()),
        }
    }

    /// The parsed attenuated cap-token of a live sub-agent — an audit accessor
    /// for verifying the sub-agent's narrowed authority (e.g. that a restricted
    /// tool was actually removed). Returns [`MultiAgentError::AgentNotFound`] if
    /// the id is not live.
    pub fn attenuated_token(&self, agent_id: &AgentId) -> Result<CapToken, MultiAgentError> {
        let agents = self.agents.read();
        let sub = agents
            .get(agent_id)
            .ok_or_else(|| self.absent_error(agent_id))?;
        Ok(sub.attenuated_token.clone())
    }

    /// Cents consumed by a live sub-agent so far, or `None` if it is not live.
    pub fn cents_used(&self, agent_id: &AgentId) -> Option<u32> {
        self.agents.read().get(agent_id).map(SubAgent::cents_used)
    }

    /// The issuer root key sub-agent cap-tokens verify against.
    pub fn root_public_key(&self) -> &PublicKey {
        &self.root
    }

    /// Classify a lookup miss: a previously-terminated id is
    /// [`MultiAgentError::AlreadyTerminated`], anything else is
    /// [`MultiAgentError::AgentNotFound`].
    fn absent_error(&self, id: &AgentId) -> MultiAgentError {
        if self.terminated.read().contains(id) {
            MultiAgentError::AlreadyTerminated(id.clone())
        } else {
            MultiAgentError::AgentNotFound(id.clone())
        }
    }
}

impl InMemoryMultiAgentRuntime<InMemoryRuntime> {
    /// Build a runtime over the §1.0 in-memory echo runtime — the Phase-1
    /// default child surface.
    pub fn in_memory(
        parent_cap_token: CapToken,
        root: PublicKey,
        parent_receipt_id: ReceiptId,
    ) -> Self {
        Self::new(
            InMemoryRuntime::new(),
            parent_cap_token,
            root,
            parent_receipt_id,
        )
    }
}

#[async_trait(?Send)]
impl<R: ChatRuntime> MultiAgentRuntime for InMemoryMultiAgentRuntime<R> {
    async fn spawn(&self, spec: SubAgentSpec) -> Result<SubAgentHandle, MultiAgentError> {
        // Derive the sub-agent's authority: start from the parent token and
        // append each narrowing rule. Checks only ever intersect, so the child
        // is strictly narrower than the parent on every axis.
        let mut token = self.parent_cap_token.clone();
        for rule in &spec.cap_token_attenuation {
            token = self
                .attenuator
                .attenuate(&token, Caveat::new(rule.clone()))?;
        }

        let parent_b64 = self.parent_cap_token.to_base64()?;
        let attenuated_b64 = token.to_base64()?;
        let session_id = SessionId::new();
        let registered_at = now_millis();

        let agent_id = spec.agent_id.clone();
        let sub = SubAgent {
            child_runtime: self.child_runtime.clone(),
            agent_id: agent_id.clone(),
            parent_cap_token: CapTokenRef(parent_b64),
            attenuated_cap_token: CapTokenRef(attenuated_b64),
            cost_envelope: spec.cost_envelope,
            cost_used: std::sync::atomic::AtomicU32::new(0),
            session_id,
            goal: spec.goal,
            parent_session_id: spec.parent_session_id,
            parent_receipt_id: self.parent_receipt_id,
            registered_at,
            attenuated_token: token,
        };

        // A re-spawn of a previously-terminated id revives it.
        self.terminated.write().remove(&agent_id);
        // TODO §5.0 Phase 2: reject duplicate live ids and enforce a hierarchy
        // depth limit; Phase 1 is last-writer-wins on a re-used live id.
        self.agents.write().insert(agent_id.clone(), sub);

        Ok(SubAgentHandle {
            agent_id,
            session_id,
            registered_at,
        })
    }

    async fn ask(
        &self,
        handle: &SubAgentHandle,
        request: SubAgentRequest,
    ) -> Result<SubAgentResponse, MultiAgentError> {
        // Reserve the budget and snapshot the child handle under a short read
        // lock, then release the lock before awaiting the child runtime.
        let (child, session_id, cap_token) = {
            let agents = self.agents.read();
            let sub = agents
                .get(&handle.agent_id)
                .ok_or_else(|| self.absent_error(&handle.agent_id))?;
            sub.try_reserve(request.max_cost_cents)?;
            (
                sub.child_runtime.clone(),
                sub.session_id,
                sub.attenuated_cap_token.clone(),
            )
        };

        let req = SubmitRequest {
            messages: vec![request.message],
            cap_token,
            session_id,
            requested_provider: None,
        };

        let result = match child.submit(req).await {
            Ok(result) => result,
            Err(err) => {
                // The turn never ran — roll the reservation back so a failed
                // ask does not permanently consume the envelope.
                if let Some(sub) = self.agents.read().get(&handle.agent_id) {
                    sub.release(request.max_cost_cents);
                }
                return Err(MultiAgentError::Runtime(err));
            }
        };

        Ok(SubAgentResponse {
            message: result.response,
            cost_used: receipt_cost(result.cost, request.max_cost_cents),
            receipt_id: result.receipt_id,
            sub_receipts: Vec::new(),
        })
    }

    async fn terminate(
        &self,
        handle: SubAgentHandle,
        reason: TerminationReason,
    ) -> Result<TerminationReceipt, MultiAgentError> {
        let sub = {
            let mut agents = self.agents.write();
            match agents.remove(&handle.agent_id) {
                Some(sub) => sub,
                None => return Err(self.absent_error(&handle.agent_id)),
            }
        };
        self.terminated.write().insert(handle.agent_id.clone());

        let total_cost = CostTuple {
            tokens_in: 0,
            tokens_out: 0,
            cents: u64::from(sub.cents_used()),
            wall_ms: 0,
            attention_score: 0.0,
        };

        Ok(TerminationReceipt {
            receipt_id: ReceiptId::new(),
            agent_id: sub.agent_id,
            reason,
            total_cost,
            terminated_at: now_millis(),
            parent_receipt_id: sub.parent_receipt_id,
        })
    }

    fn list(&self) -> Vec<SubAgentHandle> {
        self.agents
            .read()
            .values()
            .map(|sub| SubAgentHandle {
                agent_id: sub.agent_id.clone(),
                session_id: sub.session_id,
                registered_at: sub.registered_at,
            })
            .collect()
    }
}
