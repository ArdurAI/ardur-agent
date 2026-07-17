use std::fmt;

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("audio processing failed: {0}")]
    ProcessingFailed(String),
    #[error("decode error: {0}")]
    DecodeError(String),
    #[error("unsupported format: {0}")]
    UnsupportedFormat(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, MediaError>;
