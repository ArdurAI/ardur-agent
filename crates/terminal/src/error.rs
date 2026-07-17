//! Error types for the terminal crate.

use thiserror::Error;

/// Shorthand result type for terminal operations.
pub type Result<T> = std::result::Result<T, TerminalError>;

/// Errors that can occur during terminal operations.
#[derive(Error, Clone, Debug, PartialEq, Eq)]
pub enum TerminalError {
    /// The requested backend is not available.
    #[error("backend not available: {0}")]
    BackendNotAvailable(String),
    /// The specified session could not be found.
    #[error("session not found: {id}")]
    SessionNotFound {
        /// The session identifier.
        id: String,
    },
    /// Command execution failed.
    #[error("command execution failed: {0}")]
    ExecutionFailed(String),
    /// The operation timed out.
    #[error("timeout after {secs}s")]
    Timeout {
        /// Timeout duration in seconds.
        secs: u64,
    },
    /// The operation was denied by policy.
    #[error("policy denied: {reason}")]
    PolicyDenied {
        /// Reason for the denial.
        reason: String,
    },
    /// A receipt-related error occurred.
    #[error("receipt error: {0}")]
    ReceiptError(String),
    /// An internal error occurred.
    #[error("internal error: {0}")]
    Internal(String),
}
