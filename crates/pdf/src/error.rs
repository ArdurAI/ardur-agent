//! Error types for the PDF crate.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, PdfError>;

#[derive(Error, Clone, Debug, PartialEq, Eq)]
pub enum PdfError {
    #[error("parse error: {0}")]
    ParseError(String),
    #[error("file not found: {0}")]
    FileNotFound(String),
    #[error("extraction failed: {0}")]
    ExtractionFailed(String),
    #[error("unsupported format: {0}")]
    UnsupportedFormat(String),
    #[error("internal error: {0}")]
    Internal(String),
}
