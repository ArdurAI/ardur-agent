use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum OpenAiCompatError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error("HTTPS required: {0}")]
    HttpsRequired(String),
    #[error("loopback URL not allowed: {0}")]
    LoopbackNotAllowed(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, OpenAiCompatError>;
