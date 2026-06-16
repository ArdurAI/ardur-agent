use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("session already exists: {0}")]
    AlreadyExists(String),
    #[error("invalid session state: {0}")]
    InvalidState(String),
    #[error("checkpoint not found: {0}")]
    CheckpointNotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, SessionError>;
