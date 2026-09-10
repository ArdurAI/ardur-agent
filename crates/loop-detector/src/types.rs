//! Domain objects: signatures, signals, evidence, halt status.

use crate::ids::{RunId, SessionId, TurnId};
use ardur_receipt::{CostTuple, Sha256Digest};
use serde::{Deserialize, Serialize};

/// A per-tool-call signature for repetition detection: the tool name plus the
/// canonical fingerprint of its arguments (see [`crate::args_fingerprint`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LoopSignature {
    /// The tool that was admitted.
    pub tool_name: String,
    /// SHA-256 over the canonicalized arguments.
    pub args_fingerprint: Sha256Digest,
}

/// What a signal trip does when its grace window expires. Ordered by
/// aggressiveness via [`OverrideAction::severity`] so budget derivation can
/// enforce that a child only ever tightens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverrideAction {
    /// Emit the detected receipt but never halt — for dev / canary profiles.
    Warn,
    /// Halt on grace expiry (the default): refuse the next tool-call admission.
    Halt,
    /// Escalate straight to kill, skipping the halt grace — for paranoid profiles.
    Kill,
}

impl OverrideAction {
    /// A monotonic severity ordinal: `Warn` (least safe) < `Halt` < `Kill`.
    /// Child budgets may only move up this scale.
    pub fn severity(self) -> u8 {
        match self {
            OverrideAction::Warn => 0,
            OverrideAction::Halt => 1,
            OverrideAction::Kill => 2,
        }
    }
}

/// Which of the three signals fired. A set-typed discriminant so
/// [`LoopDetectorState`](crate::LoopDetectorState) can track active trips without
/// double-counting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SignalKind {
    /// The same tool + args admitted `N` times in `M` turns.
    SameToolSameArgs,
    /// `K` consecutive turns with no progress receipt.
    NoProgress,
    /// Per-turn cost accelerating by `R` for `W` windows.
    CostAcceleration,
}

/// A tripped signal with the evidence detail that names *why* it tripped.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Signal {
    /// Repetition: the offending signature and how many times it recurred.
    SameToolSameArgs {
        /// The signature that recurred past threshold.
        signature: LoopSignature,
        /// The observed occurrence count in the window.
        count: u32,
    },
    /// Stalled: how many consecutive turns produced no progress receipt.
    NoProgress {
        /// Consecutive no-progress turns observed.
        consecutive_turns: u32,
    },
    /// Cost blow-up: the observed growth ratio and how many windows sustained it.
    CostAcceleration {
        /// The most recent window's growth ratio.
        ratio: f32,
        /// Consecutive accelerating windows observed.
        consecutive_windows: u32,
    },
}

impl Signal {
    /// The set-discriminant for this signal.
    pub fn kind(&self) -> SignalKind {
        match self {
            Signal::SameToolSameArgs { .. } => SignalKind::SameToolSameArgs,
            Signal::NoProgress { .. } => SignalKind::NoProgress,
            Signal::CostAcceleration { .. } => SignalKind::CostAcceleration,
        }
    }
}

/// The evidence a halt or kill carries: enough to reconstruct why the detector
/// fired. Its canonical digest anchors the on-chain halt receipt
/// (see [`crate::evidence_payload_digest`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoopEvidence {
    /// The session the run belongs to.
    pub session_id: SessionId,
    /// The run that tripped.
    pub run_id: RunId,
    /// The signal that fired.
    pub signal: Signal,
    /// Hashes of the receipts that drove the trip (the `N` repeated admissions,
    /// or the turns in the no-progress/cost windows).
    pub offending_receipts: Vec<Sha256Digest>,
    /// The per-turn cost trajectory leading to the trip, for operator rendering.
    pub cost_trajectory: Vec<(TurnId, CostTuple)>,
}

/// Why a run was killed (terminal escalation).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum KillReason {
    /// More than one signal was active simultaneously.
    MultiSignal {
        /// The signal kinds that were jointly active.
        signals: Vec<SignalKind>,
    },
    /// The run kept requesting admissions after it was halted.
    ContinuedAfterHalt,
    /// Cost accelerated at emergency grade (double the base ratio) — brake now.
    EmergencyCostAcceleration,
}

/// The health of a run's detector state. A strict progression:
/// `Healthy → Detected → Halted → Killed`; only an operator override returns a
/// `Halted` run to `Healthy`, and `Killed` is terminal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum HaltStatus {
    /// No signal is active.
    Healthy,
    /// A signal tripped; the grace window is open.
    Detected {
        /// The signal that tripped.
        signal: Signal,
        /// The turn the grace window opened on.
        since_turn: TurnId,
    },
    /// Grace expired; the next admission is refused.
    Halted {
        /// The signal that drove the halt.
        signal: Signal,
    },
    /// The run was torn down.
    Killed {
        /// Why it was killed.
        reason: KillReason,
    },
}

/// The detector's verdict for a single observation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DetectorVerdict {
    /// No signal active — proceed normally.
    Continue,
    /// A signal tripped; the run continues within grace so it (or an operator)
    /// can react.
    SignalTripped {
        /// The tripped signal.
        signal: Signal,
        /// The evidence for the trip.
        evidence: LoopEvidence,
    },
    /// Grace expired; the runtime must refuse the next admission.
    HaltRequired {
        /// The signal that drove the halt.
        signal: Signal,
        /// The evidence for the halt.
        evidence: LoopEvidence,
    },
    /// The runtime must tear the run down.
    KillRequired {
        /// Why the kill escalated.
        reason: KillReason,
        /// The evidence for the kill.
        evidence: LoopEvidence,
    },
}

/// A whitelist rule that suppresses same-tool-same-args counting for a
/// legitimately-repetitive tool. Whitelisting is per-signal: a whitelisted tool
/// still contributes to the no-progress and cost-acceleration signals.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhitelistEntry {
    /// The tool this entry exempts.
    pub tool_name: String,
    /// How the exemption applies.
    pub kind: WhitelistKind,
}

/// The exemption mode for a [`WhitelistEntry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WhitelistKind {
    /// A status-poll tool: exempt when the caller-supplied polling key matches
    /// across calls (repeatedly asking "is job X done?" is progress-shaped).
    Polling,
    /// A pagination tool: exempt when the cursor differs across calls (walking
    /// pages is progress even though the tool + most args are identical).
    Pagination,
    /// Unconditionally exempt from repetition counting.
    BlanketExempt,
}
