//! The per-match injection signal — what was matched, by which pattern, and
//! how strongly it indicates an injection attempt.

use serde::{Deserialize, Serialize};

/// The class of injection a flag belongs to. A single scan can raise flags
/// across several categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FlagCategory {
    /// Attempts to override or discard prior/system instructions
    /// (e.g. "ignore all previous instructions").
    InstructionOverride,
    /// Attempts to reassign the model's role or persona
    /// (e.g. "you are now a …", "pretend to be …").
    RoleHijack,
    /// Abuse of chat/template delimiters or role markers
    /// (e.g. `<|im_start|>`, `[[INST]]`, `</system>`).
    DelimiterAbuse,
    /// Attempts to extract secrets or sensitive data
    /// (e.g. "exfiltrate the api key", "print my password").
    DataExfiltration,
    /// Known jailbreak invocations (e.g. "DAN mode", "do anything now").
    JailbreakAttempt,
}

/// A single pattern match raised during a scan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InjectionFlag {
    /// Stable identifier of the pattern that matched
    /// (e.g. `"ignore_previous_instructions"`).
    pub pattern_id: String,
    /// The exact substring of the scanned content that matched.
    pub matched_text: String,
    /// How strongly this match indicates an injection attempt, in `0.0..=1.0`.
    pub confidence: f32,
    /// The injection class this match belongs to.
    pub category: FlagCategory,
}
