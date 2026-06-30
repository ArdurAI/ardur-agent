//! Error types for the search crate.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, SearchError>;

#[derive(Error, Clone, Debug, PartialEq, Eq)]
pub enum SearchError {
    #[error("provider not available: {0}")]
    ProviderNotAvailable(String),
    #[error("all providers failed")]
    AllProvidersFailed,
    #[error("domain blocked: {domain}")]
    DomainBlocked { domain: String },
    #[error("domain not allowed: {domain}")]
    DomainNotAllowed { domain: String },
    #[error("search failed: {0}")]
    SearchFailed(String),
    #[error("rate limited")]
    RateLimited,
    #[error("receipt error: {0}")]
    ReceiptError(String),
    #[error("internal error: {0}")]
    Internal(String),
}
