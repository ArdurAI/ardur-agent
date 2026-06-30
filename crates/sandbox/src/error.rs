//! Error types for the sandbox crate.

use thiserror::Error;

/// The result type for sandbox operations.
pub type Result<T> = std::result::Result<T, SandboxError>;

/// Errors that can occur during sandboxed execution.
#[derive(Error, Clone, Debug, PartialEq, Eq)]
pub enum SandboxError {
    /// The language is not supported.
    #[error("unsupported language: {0}")]
    UnsupportedLanguage(String),

    /// The code execution timed out.
    #[error("execution timed out after {timeout_secs}s")]
    Timeout {
        /// The timeout in seconds.
        timeout_secs: u64,
    },

    /// The code attempted to escape the sandbox.
    #[error("sandbox escape detected: {reason}")]
    EscapeDetected {
        /// Why the escape was detected.
        reason: String,
    },

    /// The sandbox process failed to start.
    #[error("process spawn failed: {0}")]
    SpawnFailed(String),

    /// The sandbox process returned a non-zero exit code.
    #[error("process exited with code {exit_code}: {stderr}")]
    ProcessFailed {
        /// The exit code.
        exit_code: i32,
        /// The stderr output.
        stderr: String,
    },

    /// A forbidden system call or operation was attempted.
    #[error("forbidden operation: {0}")]
    ForbiddenOperation(String),

    /// A general internal error.
    #[error("internal error: {0}")]
    Internal(String),
}
