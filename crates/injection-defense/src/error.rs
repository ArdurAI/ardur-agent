//! Typed failure surface for the injection-defense layer.

use thiserror::Error;

/// Errors a filter can surface while scanning content.
#[derive(Debug, Error)]
pub enum FilterError {
    /// A pattern's regular expression failed to compile.
    #[error("regex compilation failed: {0}")]
    RegexCompilation(String),

    /// The scan exceeded its time budget. Reserved for Phase 2's bounded
    /// ML-backed scanner; the Phase 1 rule engine is synchronous and fast
    /// enough that it never trips this.
    #[error("scan timed out")]
    Timeout,

    /// The content handed to the filter could not be scanned (e.g. a tool
    /// output that does not serialize to a scannable string).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// An unexpected internal failure.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}
