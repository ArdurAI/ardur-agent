//! A wrapper filter that downgrades weak (below-threshold) matches from a bare
//! `Allow` to an `AllowWithSanitization` that redacts the matched substrings.

use async_trait::async_trait;

use crate::content::ScannableContent;
use crate::error::FilterError;
use crate::filter::{FilterId, InjectionFilter};
use crate::result::{ScanResult, Verdict};

/// The placeholder a matched substring is replaced with when sanitizing.
pub const REDACTION: &str = "[REDACTED]";

/// Wraps an inner [`InjectionFilter`] and, when the inner verdict is a clean
/// `Allow` that nonetheless raised below-threshold flags, rewrites the content
/// with each matched substring replaced by [`REDACTION`].
///
/// Decision order:
/// - inner `Block` → returned unchanged.
/// - inner `Allow` with no flags → returned unchanged.
/// - inner `Allow` with flags below the inner threshold →
///   `AllowWithSanitization`.
/// - anything else → returned unchanged.
///
/// Phase 2 (see `// TODO §11.16 Phase 2`) replaces blunt redaction with
/// context-aware rewriting.
#[derive(Debug, Clone)]
pub struct SanitizingFilter<F: InjectionFilter> {
    inner: F,
    filter_id: FilterId,
}

impl<F: InjectionFilter> SanitizingFilter<F> {
    /// Wrap `inner`. The wrapper's id is the inner id suffixed with
    /// `+sanitize`.
    pub fn new(inner: F) -> Self {
        let filter_id = FilterId::new(format!("{}+sanitize", inner.filter_id()));
        Self { inner, filter_id }
    }

    /// A reference to the wrapped filter.
    pub fn inner(&self) -> &F {
        &self.inner
    }
}

#[async_trait]
impl<F: InjectionFilter> InjectionFilter for SanitizingFilter<F> {
    async fn scan(&self, content: &ScannableContent) -> Result<ScanResult, FilterError> {
        let result = self.inner.scan(content).await?;

        // Blocked or clean → pass the inner verdict straight through.
        if matches!(result.verdict, Verdict::Block { .. }) || result.flags.is_empty() {
            return Ok(result);
        }

        // Flags raised but none reached the block threshold → sanitize.
        if result.confidence < self.inner.confidence_threshold() {
            let mut sanitized = content.scannable_text()?;
            for flag in &result.flags {
                if !flag.matched_text.is_empty() {
                    sanitized = sanitized.replace(&flag.matched_text, REDACTION);
                }
            }
            return Ok(ScanResult {
                verdict: Verdict::AllowWithSanitization { sanitized },
                ..result
            });
        }

        Ok(result)
    }

    fn filter_id(&self) -> FilterId {
        self.filter_id.clone()
    }

    fn confidence_threshold(&self) -> f32 {
        self.inner.confidence_threshold()
    }
}
