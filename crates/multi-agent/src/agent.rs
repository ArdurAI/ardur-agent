//! The [`SubAgent`] — a child [`ChatRuntime`] wrapped with an attenuated
//! cap-token, an isolated session, and a lifetime budget meter.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use ardur_cap_token::CapToken;
use ardur_cost_gate::CostEnvelope;
use ardur_receipt::UnixTsMillis;
use ardur_runtime::{CapTokenRef, ChatRuntime, ReceiptId, SessionId};

use crate::AgentId;
use crate::error::MultiAgentError;

/// One delegated sub-agent. It owns a shared handle to the child
/// [`ChatRuntime`] (the model surface), the cap-token narrowed from its
/// parent's authority, the isolated session its turns run in, and an atomic
/// cents meter enforcing its lifetime [`CostEnvelope`].
///
/// The child runtime is held generically (`Arc<R>`) rather than as the spec's
/// `Box<dyn ChatRuntime>`: §1.0's `ChatRuntime::submit` returns a
/// return-position `impl Future`, which makes the trait neither dyn-compatible
/// nor Send-bounded, so a concrete `R` is the only way to wrap it. The `Arc`
/// lets an in-flight `ask` clone out the runtime handle and release the
/// registry lock before awaiting the child.
pub struct SubAgent<R: ChatRuntime> {
    /// The wrapped child chat runtime (shared; stateless across sub-agents in
    /// Phase 1).
    pub child_runtime: Arc<R>,
    /// This sub-agent's id.
    pub agent_id: AgentId,
    /// The parent agent's authority, in cap-token wire form.
    pub parent_cap_token: CapTokenRef,
    /// The narrowed authority this sub-agent runs under, in cap-token wire form.
    pub attenuated_cap_token: CapTokenRef,
    /// The sub-agent's lifetime cost ceiling.
    pub cost_envelope: CostEnvelope,
    /// Cents consumed so far (reserved at each ask). The hard ceiling is
    /// `cost_envelope.cents_max`.
    pub cost_used: AtomicU32,

    /// The isolated session the sub-agent's turns run in.
    pub(crate) session_id: SessionId,
    /// What this sub-agent was delegated to do.
    pub(crate) goal: String,
    /// The parent session whose receipt chain this sub-agent links into.
    pub(crate) parent_session_id: SessionId,
    /// The parent receipt this sub-agent's termination links back to.
    pub(crate) parent_receipt_id: ReceiptId,
    /// When this sub-agent was registered.
    pub(crate) registered_at: UnixTsMillis,
    /// The parsed attenuated cap-token, cached for the audit accessor so the
    /// base64 form need not be re-parsed on every inspection.
    pub(crate) attenuated_token: CapToken,
}

impl<R: ChatRuntime> SubAgent<R> {
    /// What this sub-agent was delegated to do.
    pub fn goal(&self) -> &str {
        &self.goal
    }

    /// The parent session this sub-agent's receipt chain links into.
    pub fn parent_session_id(&self) -> SessionId {
        self.parent_session_id
    }

    /// Cents consumed by this sub-agent so far.
    pub fn cents_used(&self) -> u32 {
        self.cost_used.load(Ordering::Acquire)
    }

    /// Atomically reserve `cents` against the lifetime envelope. Returns
    /// [`MultiAgentError::BudgetExhausted`] (without mutating the meter) if the
    /// reservation would exceed the ceiling. A compare-and-swap loop makes the
    /// check-and-charge a single atomic step so concurrent asks against the same
    /// sub-agent can never collectively overspend.
    pub(crate) fn try_reserve(&self, cents: u32) -> Result<(), MultiAgentError> {
        let ceiling = self.cost_envelope.cents_max;
        let mut used = self.cost_used.load(Ordering::Acquire);
        loop {
            let next = used.saturating_add(cents);
            if next > ceiling {
                return Err(MultiAgentError::BudgetExhausted {
                    agent: self.agent_id.clone(),
                    used,
                    envelope: ceiling,
                });
            }
            match self.cost_used.compare_exchange_weak(
                used,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => used = observed,
            }
        }
    }

    /// Credit `cents` back to the meter — used to roll back a reservation when
    /// the child runtime rejects the turn after the reserve committed.
    pub(crate) fn release(&self, cents: u32) {
        // Saturating, not wrapping (gh#367): across a terminate+respawn id
        // reuse the release can run against a fresh counter, and a wrapping
        // `fetch_sub` would turn 0 - n into ~4 billion cents of phantom spend,
        // corrupting the new agent's envelope in the direction that permits
        // more spending rather than less.
        let _ = self
            .cost_used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                Some(used.saturating_sub(cents))
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ardur_cap_token::{BiscuitCapTokenIssuer, CapScope, CapTokenIssuer, HolderId, KeyPair};
    use ardur_runtime::InMemoryRuntime;

    /// A real issued token — `CapToken` has no test constructor, so mint one
    /// exactly the way `tests/common` does.
    fn issued_token() -> CapToken {
        BiscuitCapTokenIssuer::new(KeyPair::new())
            .issue(
                HolderId("spiffe://ardur/agent/meter-probe".to_string()),
                CapScope {
                    audience: "agent".to_string(),
                    expires_unix: 4_102_444_800,
                    budget_remaining: 1_000,
                    tool_allowlist: vec!["chat.submit".to_string()],
                },
            )
            .expect("issue probe token")
    }

    /// A minimal sub-agent whose meter starts at zero.
    ///
    /// Built by struct literal because `SubAgent` has no constructor: the
    /// runtime builds it inline. Only the cost meter matters here.
    fn metered(used: u32) -> SubAgent<InMemoryRuntime> {
        let token = CapTokenRef(String::new());
        SubAgent {
            child_runtime: Arc::new(InMemoryRuntime::new()),
            agent_id: AgentId::new("meter-probe"),
            parent_cap_token: token.clone(),
            attenuated_cap_token: token,
            cost_envelope: CostEnvelope::default(),
            cost_used: AtomicU32::new(used),
            session_id: SessionId::new(),
            goal: "probe".to_string(),
            parent_session_id: SessionId::new(),
            parent_receipt_id: ReceiptId::new(),
            registered_at: UnixTsMillis(0),
            attenuated_token: issued_token(),
        }
    }

    /// gh#367 — releasing more than was reserved must not wrap.
    #[test]
    fn releasing_more_than_reserved_saturates_at_zero() {
        let agent = metered(0);
        agent.release(500);
        assert_eq!(
            agent.cents_used(),
            0,
            "an over-release must floor at zero, never wrap to ~4 billion"
        );
    }

    /// A normal reserve/release round-trip still nets out.
    #[test]
    fn a_release_of_what_was_reserved_returns_to_zero() {
        let agent = metered(300);
        agent.release(300);
        assert_eq!(agent.cents_used(), 0);
    }
}
