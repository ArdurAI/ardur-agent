//! The crate's single typed-error surface.
//!
//! Every fallible operation on a [`MultiAgentRuntime`](crate::MultiAgentRuntime)
//! returns [`MultiAgentError`]. The variants map onto the §5.0 sub-agent
//! lifecycle failure modes the parent must distinguish: an unknown agent is not
//! an exhausted budget, a cap-token attenuation failure is not a child-runtime
//! failure, and re-terminating is not a missing agent.

use ardur_cap_token::CapTokenError;
use ardur_runtime::RuntimeError;

use crate::AgentId;

/// All ways a sub-agent runtime operation can fail.
#[derive(Debug, thiserror::Error)]
pub enum MultiAgentError {
    /// No sub-agent is registered under this id (and none was terminated under
    /// it — that is [`MultiAgentError::AlreadyTerminated`]).
    #[error("sub-agent not found: {0}")]
    AgentNotFound(AgentId),

    /// Admitting the requested cost would push the sub-agent past its lifetime
    /// [`CostEnvelope`](ardur_cost_gate::CostEnvelope) ceiling. Reported before
    /// the child runtime is invoked, so an over-budget ask never reaches the
    /// model.
    #[error(
        "sub-agent {agent} budget exhausted: used {used}c + request would exceed envelope {envelope}c"
    )]
    BudgetExhausted {
        /// The sub-agent whose budget would be exceeded.
        agent: AgentId,
        /// Cents already consumed by this sub-agent.
        used: u32,
        /// The sub-agent's lifetime cents ceiling.
        envelope: u32,
    },

    /// Deriving or serializing the sub-agent's attenuated cap-token failed.
    #[error("cap-token error: {0}")]
    CapTokenError(#[from] CapTokenError),

    /// The wrapped child [`ChatRuntime`](ardur_runtime::ChatRuntime) rejected
    /// the turn.
    #[error("child runtime error: {0}")]
    Runtime(#[from] RuntimeError),

    /// The sub-agent was already terminated; its handle can no longer be used.
    #[error("sub-agent already terminated: {0}")]
    AlreadyTerminated(AgentId),

    /// An otherwise-unclassified internal failure (e.g. a Biscuit attenuation
    /// append that returned an opaque error).
    #[error("internal multi-agent error: {0}")]
    Internal(#[from] anyhow::Error),
}
