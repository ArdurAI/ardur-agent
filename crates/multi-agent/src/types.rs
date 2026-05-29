//! The value types of the sub-agent protocol: the ids the parent addresses a
//! sub-agent by, the spawn spec, the lifecycle handle, the request/response
//! envelope a turn is carried in, and the termination receipt that links a
//! sub-agent's audit chain back into its parent's.
//!
//! Cost and timestamp fields speak the §11.14 receipt vocab ([`CostTuple`],
//! [`UnixTsMillis`]) so a [`TerminationReceipt`] is shaped like every other
//! receipt in the system (§5.7, ADR-Phase3-234..237).

use std::fmt;

use ardur_cap_token::AttenuationRule;
use ardur_cost_gate::CostEnvelope;
use ardur_receipt::{CostTuple, UnixTsMillis};
use ardur_runtime::{ChatMessage, ReceiptId, SessionId};
use serde::{Deserialize, Serialize};

/// Stable identifier of a sub-agent within a parent's registry (e.g.
/// `"researcher-1"`). Opaque, caller-chosen, and the lookup key into the
/// runtime.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub String);

impl AgentId {
    /// Wrap a string id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<T: Into<String>> From<T> for AgentId {
    fn from(s: T) -> Self {
        Self(s.into())
    }
}

/// The principal that initiated an action against a sub-agent — used to
/// attribute a [`TerminationReason::Cancelled`]. Opaque string identifier,
/// typically a SPIFFE-style URI mirroring the cap-token holder identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PrincipalRef(pub String);

impl fmt::Display for PrincipalRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The recipe for spawning one sub-agent: who it is, what it is for, how its
/// authority is narrowed from the parent's, its lifetime budget, and the parent
/// session its receipt chain links back to.
#[derive(Clone, Debug)]
pub struct SubAgentSpec {
    /// The id the sub-agent will be registered and addressed under.
    pub agent_id: AgentId,
    /// Free-text description of what this sub-agent is delegated to do.
    pub goal: String,
    /// The strictly-narrowing rules applied to the parent's cap-token to derive
    /// the sub-agent's authority (passed straight through to the §11.14
    /// attenuator). Applied in order; each can only shrink authority.
    pub cap_token_attenuation: Vec<AttenuationRule>,
    /// The maximum cost this sub-agent may consume across its entire lifetime.
    pub cost_envelope: CostEnvelope,
    /// The parent session whose receipt chain a termination receipt links into.
    pub parent_session_id: SessionId,
}

/// A cheap-to-clone handle to a spawned sub-agent. It is the lookup key the
/// parent presents back to [`ask`](crate::MultiAgentRuntime::ask) and
/// [`terminate`](crate::MultiAgentRuntime::terminate); it carries no authority
/// of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubAgentHandle {
    /// The sub-agent's id.
    pub agent_id: AgentId,
    /// The isolated session the sub-agent's turns run in.
    pub session_id: SessionId,
    /// When the sub-agent was registered.
    pub registered_at: UnixTsMillis,
}

/// A single chat turn the parent sends to a sub-agent, with the maximum cost the
/// parent authorizes this turn to consume from the sub-agent's envelope.
#[derive(Clone, Debug)]
pub struct SubAgentRequest {
    /// The message to send to the sub-agent.
    pub message: ChatMessage,
    /// The maximum cost (in cents) this turn may reserve against the
    /// sub-agent's [`CostEnvelope`]. Reserved before the child runs; an ask
    /// that would exceed the envelope is rejected up front.
    pub max_cost_cents: u32,
}

/// A sub-agent's reply to a [`SubAgentRequest`]: the assistant message, the cost
/// charged, and the receipt ids the turn produced.
#[derive(Clone, Debug)]
pub struct SubAgentResponse {
    /// The sub-agent's response message.
    pub message: ChatMessage,
    /// The cost this turn charged against the sub-agent's envelope.
    pub cost_used: CostTuple,
    /// The receipt id of this turn (the child runtime's turn receipt).
    pub receipt_id: ReceiptId,
    /// Any nested receipts the turn produced (e.g. tool-call receipts). Empty
    /// in Phase 1.
    // TODO §5.0 Phase 2: populate from the sub-agent's tool-call receipt chain
    // once the tool layer (§6.0) emits per-invocation receipts.
    pub sub_receipts: Vec<ReceiptId>,
}

/// Why a sub-agent was terminated. Recorded verbatim on the
/// [`TerminationReceipt`] so the parent's audit chain captures the cause.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminationReason {
    /// The sub-agent finished its delegated work normally.
    Completed,
    /// The sub-agent exhausted its lifetime budget envelope.
    BudgetExhausted,
    /// The sub-agent exceeded its wall-clock allowance.
    TimedOut {
        /// Wall-clock milliseconds elapsed at termination.
        wall_ms: u64,
    },
    /// A principal cancelled the sub-agent before it completed.
    Cancelled {
        /// Who initiated the cancellation.
        by: PrincipalRef,
    },
    /// The sub-agent terminated because of an error.
    ErrorOccurred(String),
}

/// The receipt emitted when a sub-agent is terminated. It links into the
/// parent's audit chain via `parent_receipt_id`, mirroring the §11.14 receipt
/// vocab (§5.7, ADR-Phase3-234..237) so the sub-agent's lifecycle is auditable
/// from the parent's chain.
#[derive(Clone, Debug)]
pub struct TerminationReceipt {
    /// Stable id of this termination receipt.
    pub receipt_id: ReceiptId,
    /// The sub-agent this receipt is about.
    pub agent_id: AgentId,
    /// Why the sub-agent was terminated.
    pub reason: TerminationReason,
    /// The total cost the sub-agent consumed over its lifetime.
    pub total_cost: CostTuple,
    /// When the sub-agent was terminated.
    pub terminated_at: UnixTsMillis,
    /// The parent receipt this links back to — the anchor in the parent's audit
    /// chain.
    pub parent_receipt_id: ReceiptId,
}
