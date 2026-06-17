use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("log not found: {0}")]
    NotFound(String),
    #[error("invalid filter: {0}")]
    InvalidFilter(String),
    #[error("export failed: {0}")]
    ExportFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, LogError>;
