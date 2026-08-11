//! The rule-based filter: a set of compiled regex signatures and the
//! [`PatternBasedFilter`] that scans content against them.
//!
//! This is the whole of Phase 1's detection. Phase 2 replaces (or augments)
//! the regex table with an ML-backed classifier behind the same
//! [`InjectionFilter`] surface.

use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use std::time::Instant;

use crate::content::ScannableContent;
use crate::error::FilterError;
use crate::filter::{FilterId, InjectionFilter};
use crate::flag::{FlagCategory, InjectionFlag};
use crate::result::{ScanResult, Verdict};

/// A single injection signature: a compiled regex plus the metadata a match
/// contributes to an [`InjectionFlag`].
#[derive(Debug, Clone)]
pub struct CompiledPattern {
    id: String,
    regex: Regex,
    category: FlagCategory,
    confidence: f32,
}

impl CompiledPattern {
    /// Compile a pattern. Fails with [`FilterError::RegexCompilation`] if the
    /// regex is invalid, or [`FilterError::InvalidInput`] if the confidence is
    /// outside `0.0..=1.0`.
    pub fn new(
        id: impl Into<String>,
        regex: &str,
        category: FlagCategory,
        confidence: f32,
    ) -> Result<Self, FilterError> {
        if !(0.0..=1.0).contains(&confidence) {
            return Err(FilterError::InvalidInput(format!(
                "confidence {confidence} out of range 0.0..=1.0"
            )));
        }
        let regex = Regex::new(regex).map_err(|e| FilterError::RegexCompilation(e.to_string()))?;
        Ok(Self {
            id: id.into(),
            regex,
            category,
            confidence,
        })
    }

    /// The pattern's stable identifier.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The injection class this pattern detects.
    pub fn category(&self) -> FlagCategory {
        self.category
    }

    /// The confidence a match contributes.
    pub fn confidence(&self) -> f32 {
        self.confidence
    }
}

/// The built-in signature specs: `(id, regex, category, confidence)`. Compiled
/// once into [`BUILTIN_PATTERNS`].
const BUILTIN_SPECS: &[(&str, &str, FlagCategory, f32)] = &[
    (
        "ignore_previous_instructions",
        r"(?i)ignore\s+(all\s+)?(previous|prior|above)\s+instructions",
        FlagCategory::InstructionOverride,
        0.9,
    ),
    (
        "system_role_hijack",
        r"(?i)you\s+are\s+now\s+a",
        FlagCategory::RoleHijack,
        0.7,
    ),
    (
        "disregard_system_prompt",
        r"(?i)disregard\s+(the|all)\s+(system\s+)?prompt",
        FlagCategory::InstructionOverride,
        0.9,
    ),
    (
        "delimiter_injection",
        // Chat-template control delimiters used to smuggle a new turn/role:
        //   - `[INST]` / `[/INST]` (Llama/Mistral), incl. the doubled `[[INST]]`
        //     and internal-whitespace forms; anchored to exactly INST so
        //     `[INSTALL]`, `[INSTRUCTIONS]`, `[INFO]`, `a[0]` do not match.
        //   - `<|im_start|>` (ChatML).
        //   - `<<SYS>>` / `<</SYS>>` (Llama system block, paired with `[INST]`).
        r"(?i)\[\s*/?\s*INST\s*\]|<\|im_start\|>|<</?SYS>>",
        FlagCategory::DelimiterAbuse,
        0.85,
    ),
    (
        "exfiltrate_secret",
        r"(?i)exfiltrate|leak\s+(api[-_ ]?key|secret|token)",
        FlagCategory::DataExfiltration,
        0.95,
    ),
    (
        "pretend_persona",
        r"(?i)pretend\s+(you\s+are|to\s+be)",
        FlagCategory::RoleHijack,
        0.65,
    ),
    (
        "dan_jailbreak_mode",
        r"(?i)DAN\s+mode|jailbreak\s+mode",
        FlagCategory::JailbreakAttempt,
        0.95,
    ),
    (
        "system_directive_delimiter",
        r"(?i)/system\s*[:>]",
        FlagCategory::DelimiterAbuse,
        0.8,
    ),
    (
        "forget_everything",
        r"(?i)forget\s+(everything|all)\s+(you|that)",
        FlagCategory::InstructionOverride,
        0.85,
    ),
    // --- Additional Phase 1 signatures (beyond the spec's minimum nine) ---
    (
        "extract_credentials",
        r"(?i)(print|show|reveal|send|dump|export)\s+(me\s+)?(my\s+|the\s+|your\s+)?(api[-_ ]?key|secret|password|token|credential)",
        FlagCategory::DataExfiltration,
        0.85,
    ),
    (
        "act_as_persona",
        r"(?i)(act|behave)\s+as\s+(if\s+you\s+(are|were)\s+|an?\s+)",
        FlagCategory::RoleHijack,
        0.7,
    ),
    (
        "do_anything_now",
        r"(?i)do\s+anything\s+now",
        FlagCategory::JailbreakAttempt,
        0.9,
    ),
    (
        "role_tag_delimiter",
        r"(?i)<\s*/?\s*(system|assistant|user)\s*>",
        FlagCategory::DelimiterAbuse,
        0.75,
    ),
    (
        "override_instructions",
        r"(?i)override\s+(your\s+)?(system\s+)?(instructions|rules|guidelines)",
        FlagCategory::InstructionOverride,
        0.88,
    ),
    (
        "bypass_safety",
        r"(?i)bypass\s+(your\s+)?(safety|content)\s+(filter|policy|guidelines)",
        FlagCategory::JailbreakAttempt,
        0.9,
    ),
];

/// The built-in injection signatures, compiled once on first use. The specs in
/// [`BUILTIN_SPECS`] are constants known to compile, so a failure here is a
/// programming error and panics.
static BUILTIN_PATTERNS: Lazy<Vec<CompiledPattern>> = Lazy::new(|| {
    BUILTIN_SPECS
        .iter()
        .map(|(id, re, cat, conf)| {
            CompiledPattern::new(*id, re, *cat, *conf)
                .expect("built-in injection pattern must compile")
        })
        .collect()
});

/// The default confidence threshold: matches at or above this block; below it
/// are sanitization candidates. Chosen so the `you_are_now_a` role-hijack
/// signature (0.7) blocks while weaker signals (e.g. `pretend`, 0.65) do not.
pub const DEFAULT_THRESHOLD: f32 = 0.7;

/// A filter that scans content against a set of compiled regex signatures.
#[derive(Debug, Clone)]
pub struct PatternBasedFilter {
    filter_id: FilterId,
    patterns: Vec<CompiledPattern>,
    threshold: f32,
}

impl PatternBasedFilter {
    /// A filter seeded with all built-in signatures and the
    /// [`DEFAULT_THRESHOLD`].
    pub fn new() -> Self {
        Self {
            filter_id: FilterId::new("pattern-based"),
            patterns: BUILTIN_PATTERNS.clone(),
            threshold: DEFAULT_THRESHOLD,
        }
    }

    /// A filter with a caller-supplied id, threshold, and signature set.
    /// Fails if the threshold is outside `0.0..=1.0`.
    pub fn with_patterns(
        filter_id: impl Into<String>,
        threshold: f32,
        patterns: Vec<CompiledPattern>,
    ) -> Result<Self, FilterError> {
        if !(0.0..=1.0).contains(&threshold) {
            return Err(FilterError::InvalidInput(format!(
                "threshold {threshold} out of range 0.0..=1.0"
            )));
        }
        Ok(Self {
            filter_id: FilterId::new(filter_id),
            patterns,
            threshold,
        })
    }

    /// A clone of the built-in signature set, for callers assembling a custom
    /// filter that extends rather than replaces the defaults.
    pub fn builtin_patterns() -> Vec<CompiledPattern> {
        BUILTIN_PATTERNS.clone()
    }

    /// The number of signatures this filter scans against.
    pub fn pattern_count(&self) -> usize {
        self.patterns.len()
    }
}

impl Default for PatternBasedFilter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl InjectionFilter for PatternBasedFilter {
    async fn scan(&self, content: &ScannableContent) -> Result<ScanResult, FilterError> {
        let start = Instant::now();
        let text = content.scannable_text()?;

        let mut flags = Vec::new();
        for pattern in &self.patterns {
            if let Some(m) = pattern.regex.find(&text) {
                flags.push(InjectionFlag {
                    pattern_id: pattern.id.clone(),
                    matched_text: m.as_str().to_string(),
                    confidence: pattern.confidence,
                    category: pattern.category,
                });
            }
        }

        let confidence = flags.iter().map(|f| f.confidence).fold(0.0_f32, f32::max);

        let blocking: Vec<&str> = flags
            .iter()
            .filter(|f| f.confidence >= self.threshold)
            .map(|f| f.pattern_id.as_str())
            .collect();

        let verdict = if blocking.is_empty() {
            Verdict::Allow
        } else {
            Verdict::Block {
                reason: format!("injection signatures matched: {}", blocking.join(", ")),
            }
        };

        Ok(ScanResult {
            verdict,
            flags,
            confidence,
            scan_duration_ms: start.elapsed().as_millis() as u32,
        })
    }

    fn filter_id(&self) -> FilterId {
        self.filter_id.clone()
    }

    fn confidence_threshold(&self) -> f32 {
        self.threshold
    }
}
