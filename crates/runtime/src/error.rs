//! The runtime's single typed-error surface.
//!
//! Every fallible operation in this crate returns [`RuntimeError`]. The
//! variants map onto the §1.0 admission/dispatch failure modes the caller must
//! distinguish: a missing token is not an expired one, and a cost rejection is
//! not a dead provider.

/// All ways a runtime or command-bus operation can fail.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// The request carried no capability token, or the referenced token could
    /// not be resolved.
    #[error("capability token missing or unresolved")]
    CapTokenMissing,

    /// The referenced capability token has expired and can no longer authorize
    /// a turn.
    #[error("capability token expired")]
    CapTokenExpired,

    /// Admitting the turn would exceed the session's configured cost ceiling.
    #[error("cost ceiling exceeded")]
    CostCeilingExceeded,

    /// The requested provider is not registered or is currently unreachable.
    #[error("provider unavailable")]
    ProviderUnavailable,

    /// No command was registered under the dispatched name.
    #[error("command not found: {0}")]
    CommandNotFound(String),

    /// An otherwise-unclassified internal failure.
    #[error("internal runtime error: {0}")]
    Internal(#[from] anyhow::Error),
}
