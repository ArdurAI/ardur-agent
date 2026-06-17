use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum CommitmentError {
    #[error("commitment not found: {0}")]
    NotFound(String),
    #[error("commitment already exists: {0}")]
    AlreadyExists(String),
    #[error("invalid commitment: {0}")]
    Invalid(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CommitmentError>;
