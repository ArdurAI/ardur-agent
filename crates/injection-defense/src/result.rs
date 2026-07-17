//! Scan verdicts and the result structs filters return.

use serde::{Deserialize, Serialize};

use crate::flag::InjectionFlag;

/// What a filter decided about a piece of content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Verdict {
    /// Forward the content unchanged.
    Allow,
    /// Forward a sanitized rewrite instead of the original.
    AllowWithSanitization {
        /// The rewritten content, safe to forward.
        sanitized: String,
    },
    /// Refuse to forward the content.
    Block {
        /// Why the content was blocked.
        reason: String,
    },
}

/// The outcome of a single filter scanning a single piece of content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanResult {
    /// The filter's decision.
    pub verdict: Verdict,
    /// Every pattern match raised during the scan.
    pub flags: Vec<InjectionFlag>,
    /// The maximum confidence across all flags (`0.0` when none matched).
    pub confidence: f32,
    /// How long the scan took, in milliseconds.
    pub scan_duration_ms: u32,
}

/// The aggregated outcome of running every filter in a [`crate::FilterRegistry`]
/// over one piece of content. The verdict is the most-restrictive across all
/// filters (any `Block` wins; otherwise any `AllowWithSanitization` wins).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CombinedScanResult {
    /// The aggregated, most-restrictive verdict.
    pub verdict: Verdict,
    /// The union of flags raised by every filter.
    pub flags: Vec<InjectionFlag>,
    /// The maximum confidence across every filter's result.
    pub confidence: f32,
    /// The individual per-filter results, in registry order.
    pub results: Vec<ScanResult>,
}
