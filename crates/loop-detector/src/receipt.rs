//! Receipt-verb vocabulary and evidence anchoring.
//!
//! The detector never signs or chains a receipt itself — the owning runtime
//! mints one through `ardur-receipt` for each verb below, chaining it onto the
//! same audit log as the tool calls that drove it. This module supplies the
//! canonical verb strings (validated against the `verb.object.state.vN` grammar)
//! and the payload digest that anchors a halt's [`LoopEvidence`] on chain.

use crate::types::LoopEvidence;
use ardur_receipt::{Sha256Digest, VerbObject};
use sha2::{Digest, Sha256};

/// The §11.13 receipt verbs. Each is emitted by the owning runtime at the point
/// named in its doc comment.
pub mod verbs {
    /// Any signal trips (grace window opens).
    pub const LOOP_DETECTED: &str = "agent.loop.detected.v1";
    /// Grace window expires; the halt fires.
    pub const LOOP_HALTED: &str = "agent.loop.halted.v1";
    /// Kill escalation — the run is torn down.
    pub const RUNAWAY_KILLED: &str = "agent.runaway.killed.v1";
    /// A tripped signal recovered within its grace window.
    pub const SIGNAL_CLEARED: &str = "agent.loop.signal_cleared.v1";
    /// An operator overrode a halt and resumed the run.
    pub const DETECTION_OVERRIDDEN: &str = "agent.loop.detection_overridden.v1";
    /// A tool-call admission was refused because a halt is active.
    pub const REFUSED_BY_LOOP_HALT: &str = "tool.call.refused_by_loop_halt.v1";
    /// A loop-budget derivation attempted to relax a field and was refused.
    pub const BUDGET_RELAXATION_REFUSED: &str =
        "cap_token.derivation.loop_budget_relaxation_refused.v1";
}

/// Build the [`VerbObject`] for one of the [`verbs`] constants.
///
/// The constants are compile-time literals that all satisfy the receipt grammar,
/// so this only fails if a caller passes a non-conforming string; callers should
/// pass a `verbs::*` constant.
pub fn verb(s: &str) -> Result<VerbObject, ardur_receipt::ReceiptError> {
    VerbObject::new(s)
}

/// The SHA-256 of a halt/kill's evidence, to place in the receipt body's
/// `payload_digest`. The evidence itself is carried off-chain (or in the receipt
/// journal); the digest makes the halt tamper-evident on the receipt chain.
pub fn evidence_payload_digest(evidence: &LoopEvidence) -> Sha256Digest {
    let bytes = serde_json::to_vec(evidence).unwrap_or_default();
    Sha256Digest(Sha256::digest(&bytes).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_verb_matches_the_receipt_grammar() {
        for v in [
            verbs::LOOP_DETECTED,
            verbs::LOOP_HALTED,
            verbs::RUNAWAY_KILLED,
            verbs::SIGNAL_CLEARED,
            verbs::DETECTION_OVERRIDDEN,
            verbs::REFUSED_BY_LOOP_HALT,
            verbs::BUDGET_RELAXATION_REFUSED,
        ] {
            assert!(verb(v).is_ok(), "verb `{v}` must satisfy the grammar");
        }
    }
}
