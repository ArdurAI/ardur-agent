use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum AutomationError {
    #[error("task not found: {0}")]
    TaskNotFound(String),
    #[error("task execution failed: {0}")]
    ExecutionFailed(String),
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, AutomationError>;
