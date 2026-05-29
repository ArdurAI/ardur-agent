//! A registry of filters that scans content through all of them and
//! aggregates their verdicts most-restrictive-wins.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::content::ScannableContent;
use crate::error::FilterError;
use crate::filter::InjectionFilter;
use crate::result::{CombinedScanResult, ScanResult, Verdict};
use crate::sanitize::REDACTION;

/// A collection of filters, scanned in registration order.
///
/// Filters are held as `Arc<dyn InjectionFilter>` (rather than `Box`) so
/// [`scan_all`](Self::scan_all) can snapshot the set under the lock and then
/// run the async scans without holding the lock across an `.await`.
pub struct FilterRegistry {
    filters: RwLock<Vec<Arc<dyn InjectionFilter>>>,
}

impl FilterRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            filters: RwLock::new(Vec::new()),
        }
    }

    /// Register a filter. Returns `self` so registrations can be chained.
    pub fn register(&self, filter: Arc<dyn InjectionFilter>) -> &Self {
        self.filters.write().push(filter);
        self
    }

    /// The number of registered filters.
    pub fn len(&self) -> usize {
        self.filters.read().len()
    }

    /// Whether no filters are registered.
    pub fn is_empty(&self) -> bool {
        self.filters.read().is_empty()
    }

    /// Scan `content` through every registered filter and aggregate the
    /// verdicts. The combined verdict is the most restrictive: any `Block`
    /// wins; otherwise any `AllowWithSanitization` wins (its sanitizations are
    /// merged by redacting every matched substring from the original);
    /// otherwise `Allow`.
    pub async fn scan_all(
        &self,
        content: &ScannableContent,
    ) -> Result<CombinedScanResult, FilterError> {
        // Snapshot under the lock, then release it before awaiting.
        let filters: Vec<Arc<dyn InjectionFilter>> = self.filters.read().clone();

        let mut results: Vec<ScanResult> = Vec::with_capacity(filters.len());
        for filter in &filters {
            results.push(filter.scan(content).await?);
        }

        let flags: Vec<_> = results.iter().flat_map(|r| r.flags.clone()).collect();
        let confidence = results.iter().map(|r| r.confidence).fold(0.0_f32, f32::max);

        let verdict = Self::aggregate_verdict(content, &results)?;

        Ok(CombinedScanResult {
            verdict,
            flags,
            confidence,
            results,
        })
    }

    fn aggregate_verdict(
        content: &ScannableContent,
        results: &[ScanResult],
    ) -> Result<Verdict, FilterError> {
        let block_reasons: Vec<&str> = results
            .iter()
            .filter_map(|r| match &r.verdict {
                Verdict::Block { reason } => Some(reason.as_str()),
                _ => None,
            })
            .collect();
        if !block_reasons.is_empty() {
            return Ok(Verdict::Block {
                reason: block_reasons.join("; "),
            });
        }

        let needs_sanitization = results
            .iter()
            .any(|r| matches!(r.verdict, Verdict::AllowWithSanitization { .. }));
        if needs_sanitization {
            let mut sanitized = content.scannable_text()?;
            for flag in results.iter().flat_map(|r| &r.flags) {
                if !flag.matched_text.is_empty() {
                    sanitized = sanitized.replace(&flag.matched_text, REDACTION);
                }
            }
            return Ok(Verdict::AllowWithSanitization { sanitized });
        }

        Ok(Verdict::Allow)
    }
}

impl Default for FilterRegistry {
    fn default() -> Self {
        Self::new()
    }
}
