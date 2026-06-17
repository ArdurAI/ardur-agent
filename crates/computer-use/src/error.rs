//! Error types for the computer-use crate.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, ComputerUseError>;

#[derive(Error, Clone, Debug, PartialEq, Eq)]
pub enum ComputerUseError {
    #[error("platform not supported: {0}")]
    PlatformNotSupported(String),
    #[error("element not found: {0}")]
    ElementNotFound(String),
    #[error("action failed: {0}")]
    ActionFailed(String),
    #[error("accessibility API error: {0}")]
    AccessibilityError(String),
    #[error("policy denied: {reason}")]
    PolicyDenied { reason: String },
    #[error("internal error: {0}")]
    Internal(String),
}
