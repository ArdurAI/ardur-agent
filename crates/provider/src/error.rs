use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider not found: {0}")]
    NotFound(String),
    #[error("provider not available: {0}")]
    NotAvailable(String),
    #[error("invalid provider configuration: {0}")]
    InvalidConfig(String),
    #[error("model not found: {0}")]
    ModelNotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ProviderError>;
