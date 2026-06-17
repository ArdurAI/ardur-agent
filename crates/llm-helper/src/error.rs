//! Error types for the LLM helper crate.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, LlmHelperError>;

#[derive(Error, Clone, Debug, PartialEq, Eq)]
pub enum LlmHelperError {
    #[error("budget exceeded: used {used} of {budget} tokens")]
    BudgetExceeded { used: u64, budget: u64 },
    #[error("provider error: {0}")]
    ProviderError(String),
    #[error("task failed: {0}")]
    TaskFailed(String),
    #[error("receipt error: {0}")]
    ReceiptError(String),
    #[error("internal error: {0}")]
    Internal(String),
}
