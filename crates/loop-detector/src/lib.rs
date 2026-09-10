//! ardur-loop-detector — the §11.13 runaway-agent detector.
//!
//! Plan family: §11.13
//! (`plans/11.13-loop-detection-runaway-agent-controls-blueprint.md`). Design
//! records: ADR-Phase3-274 (cap-token-encoded [`LoopBudget`] with monotonic
//! attenuation) and ADR-Phase3-275 (`loop_detector_admin` scope for the
//! operator override).
//!
//! A stuck agent — one calling the same tool with the same arguments turn after
//! turn, or spending tokens without producing any durable artifact, or bloating
//! its context so cost accelerates super-linearly — drains a budget long before
//! the §11.14 cost ceiling refuses. The cost ceiling is the last guard; this
//! crate is the early one. It watches every tool-call admission and every turn
//! boundary and trips on **any one of three signals**:
//!
//! 1. [`SignalKind::SameToolSameArgs`] — the same [`LoopSignature`] (tool name +
//!    canonical args fingerprint) admitted `N` times inside an `M`-turn window.
//! 2. [`SignalKind::NoProgress`] — `K` consecutive turns with no progress
//!    receipt (memory write, channel-outbound, checkpoint, or child completion).
//! 3. [`SignalKind::CostAcceleration`] — per-turn cost growing by a factor of
//!    `R` for `W` consecutive windows.
//!
//! A trip opens a grace window ([`DetectorVerdict::SignalTripped`]); if the
//! window expires without the signal clearing, [`LoopDetector::check_grace_expiry`]
//! escalates to a halt ([`DetectorVerdict::HaltRequired`]), and multiple
//! simultaneous signals — or an emergency-grade cost spike — escalate straight
//! to a kill ([`DetectorVerdict::KillRequired`]). Every escalation carries
//! [`LoopEvidence`]: the offending receipt hashes and the per-turn cost
//! trajectory, so an operator auditing a halt sees exactly what tripped it.
//!
//! # Thresholds travel in the cap-token
//!
//! The active thresholds are a [`LoopBudget`] carried on the run's cap-token.
//! [`derive_for_loop_budget`] narrows a parent budget for a child run: every
//! field may be *tightened* (lower `N`, `K`, `W`, `R`, grace) but never
//! *relaxed* — a relaxation attempt is a hard [`BudgetError::RelaxationAttempted`].
//! §5.0's child-mission derivation calls this so a sub-agent inherits, and can
//! only shrink, its parent's loop budget.
//!
//! # Substrate reuse, not re-derivation
//!
//! The detector produces verdicts and [`HaltReport`] / [`KillReport`] values
//! carrying a [`VerbObject`](ardur_receipt::VerbObject) and an evidence payload
//! digest; it does **not** sign or chain receipts itself — the owning runtime
//! mints them through `ardur-receipt` exactly as it does for every other verb,
//! so a halt sits on the same hash-chained audit log as the tool calls that
//! drove it. The cost-acceleration signal reads the §11.14
//! [`CostTuple`](ardur_receipt::CostTuple) the runtime already computes.
//!
//! # What this crate is not
//!
//! The detector runs synchronously on the admission hot path, so it makes no
//! language-model call and does no I/O — signal checks are pure window
//! inspection and hash comparison. State persistence (writing
//! [`LoopDetectorState`] to §7.0 memory for crash-resilient detection) and the
//! §4.1 daemon IPC for live inspection are the owning runtime's job; this crate
//! hands it a serializable state value and the verdict.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod budget;
mod detector;
mod error;
mod fingerprint;
mod halter;
mod ids;
mod receipt;
mod types;
mod whitelist;

pub use budget::{LoopBudget, LoopBudgetRequest, derive_for_loop_budget};
pub use detector::{
    InMemoryLoopDetector, LoopDetector, LoopDetectorState, ToolAdmission, TurnRecord,
};
pub use error::{BudgetError, DetectorError, HalterError};
pub use fingerprint::args_fingerprint;
pub use halter::{
    HaltReport, InMemoryRunawayHalter, KillReport, OverrideReport, OverrideRequest, RunawayHalter,
};
pub use ids::{RunId, SessionId, TurnId};
pub use receipt::{evidence_payload_digest, verbs};
pub use types::{
    DetectorVerdict, HaltStatus, KillReason, LoopEvidence, LoopSignature, OverrideAction, Signal,
    SignalKind, WhitelistEntry, WhitelistKind,
};
pub use whitelist::{WhitelistEvaluator, evaluate_whitelist};

/// Sealing so the detector, halter, and budget-deriver logic is single-sourced
/// and auditable — external crates observe the traits but cannot supply an
/// alternative implementation of the safety mechanism.
mod sealed {
    /// Marker supertrait; implemented only for this crate's own types.
    pub trait Sealed {}
}
