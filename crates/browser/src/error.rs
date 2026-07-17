//! Error types for the browser automation crate.

use thiserror::Error;

/// The result type for browser operations.
pub type Result<T> = std::result::Result<T, BrowserError>;

/// Errors that can occur during browser automation.
#[derive(Error, Clone, Debug, PartialEq, Eq)]
pub enum BrowserError {
    /// The requested action is not allowed by the current policy.
    #[error("policy denied: {reason}")]
    PolicyDenied {
        /// Human-readable reason for the denial.
        reason: String,
    },

    /// A CDP protocol error.
    #[error("CDP error: {0}")]
    CdpError(String),

    /// The browser is not connected.
    #[error("browser not connected")]
    NotConnected,

    /// An element was not found on the page.
    #[error("element not found: {selector}")]
    ElementNotFound {
        /// The CSS selector that failed to match.
        selector: String,
    },

    /// Navigation failed (e.g. 404, timeout).
    #[error("navigation failed: {url} — {status}")]
    NavigationFailed {
        /// The URL that failed to load.
        url: String,
        /// The HTTP status or error description.
        status: String,
    },

    /// A prompt injection was detected and blocked.
    #[error("prompt injection blocked: {reason}")]
    InjectionBlocked {
        /// Why the injection was blocked.
        reason: String,
    },

    /// Receipt generation failed.
    #[error("receipt error: {0}")]
    ReceiptError(String),

    /// A general internal error.
    #[error("internal error: {0}")]
    Internal(String),
}
