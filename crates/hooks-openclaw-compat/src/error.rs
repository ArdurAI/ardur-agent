use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum HookCompatError {
    #[error("hook not found: {0}")]
    NotFound(String),
    #[error("hook execution failed: {0}")]
    ExecutionFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, HookCompatError>;
