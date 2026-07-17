//! The tool layer's typed-error surfaces: [`ToolError`] for an invocation that
//! fails, and [`RegistryError`] for a registration that is rejected.

use crate::capability::Capability;
use crate::tool::ToolId;

/// Every way a [`Tool::invoke`](crate::Tool::invoke) call can fail.
///
/// The variants name the *tool-independent* failure classes the runtime's
/// admission and receipt logic switches on; an opaque, tool-specific failure is
/// funnelled through [`ToolError::Internal`].
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The session's cap-token did not grant a capability the tool requires.
    #[error("capability denied: {0:?}")]
    CapabilityDenied(Capability),

    /// The supplied arguments did not match the tool's input schema.
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),

    /// The tool refused the request under its own configured policy — distinct
    /// from [`ToolError::CapabilityDenied`], which is the *authorization* layer
    /// withholding a capability. `Denied` is the tool itself declining: a shell
    /// command outside its allowlist, or a file path that escapes its root.
    #[error("denied: {reason}")]
    Denied {
        /// Why the tool refused to run the request.
        reason: String,
    },

    /// The tool ran but failed to produce a result.
    #[error("tool execution failed: {0}")]
    ExecutionFailed(String),

    /// The tool's output exceeded the maximum size the runtime will accept.
    #[error("output too large: {actual} bytes exceeds the {max}-byte ceiling")]
    OutputTooLarge {
        /// The size the tool tried to return, in bytes.
        actual: usize,
        /// The maximum size the runtime accepts, in bytes.
        max: usize,
    },

    /// The tool did not complete within its time budget.
    #[error("tool timed out")]
    Timeout,

    /// Running the tool would exceed its
    /// [`cost_budget_cents`](crate::ToolContext::cost_budget_cents) ceiling.
    #[error("tool invocation exceeds its cost ceiling")]
    CostCeilingExceeded,

    /// An unexpected internal failure, carrying the underlying error verbatim.
    #[error("internal tool error: {0}")]
    Internal(#[from] anyhow::Error),
}

/// Every way a [`ToolRegistry::register`](crate::ToolRegistry::register) call
/// can be rejected.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// A tool with this id is already registered. Registration is keyed by
    /// [`ToolId`]; the registry refuses to silently replace an existing entry.
    #[error("a tool is already registered under id `{0}`")]
    DuplicateId(ToolId),
}
