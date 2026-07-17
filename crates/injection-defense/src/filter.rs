//! The [`InjectionFilter`] contract and its [`FilterId`] newtype.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::content::ScannableContent;
use crate::error::FilterError;
use crate::result::ScanResult;

/// A stable identifier for a filter instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FilterId(pub String);

impl FilterId {
    /// Construct a `FilterId` from anything string-like.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for FilterId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A filter that scans inbound content for prompt-injection patterns before
/// the runtime forwards it to a provider.
///
/// The trait is object-safe so a [`crate::FilterRegistry`] can hold a
/// heterogeneous `Vec<Box<dyn InjectionFilter>>`.
#[async_trait]
pub trait InjectionFilter: Send + Sync {
    /// Scan one piece of content and decide whether to allow, sanitize, or
    /// block it.
    async fn scan(&self, content: &ScannableContent) -> Result<ScanResult, FilterError>;

    /// This filter's stable identifier.
    fn filter_id(&self) -> FilterId;

    /// The confidence at or above which a match is treated as a block-worthy
    /// injection, in `0.0..=1.0`. Matches below this are candidates for
    /// sanitization rather than blocking.
    fn confidence_threshold(&self) -> f32;
}
