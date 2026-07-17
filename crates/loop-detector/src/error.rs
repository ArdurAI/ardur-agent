//! Typed failure surfaces for the detector, halter, and budget deriver.

use thiserror::Error;

/// A [`crate::LoopDetector`] operation failed. Detection is pure and local, so
/// these are structural misuse errors, not runtime faults.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DetectorError {
    /// [`crate::LoopDetector::check_grace_expiry`] was called on a state that is
    /// not in a `Detected` phase — there is no grace window to evaluate.
    #[error("no grace window is open (halt status is not Detected)")]
    NoGraceWindow,
    /// A turn id moved backward relative to the state's last observed turn. The
    /// detector's clock must be monotonic per session.
    #[error("turn {observed} precedes the last observed turn {last}")]
    NonMonotonicTurn {
        /// The turn id that was supplied.
        observed: u64,
        /// The most recent turn the state had already recorded.
        last: u64,
    },
}

/// A [`crate::derive_for_loop_budget`] attenuation failed.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BudgetError {
    /// A child budget request tried to *relax* a field (raise a count/window/
    /// ratio, or widen the override action). Loop budgets only ever tighten.
    #[error(
        "loop-budget derivation would relax field `{field}` ({requested} is looser than parent {parent})"
    )]
    RelaxationAttempted {
        /// The budget field the request tried to loosen.
        field: &'static str,
        /// The value the child requested, rendered for the operator.
        requested: String,
        /// The parent value that bounds it, rendered for the operator.
        parent: String,
    },
}

/// A [`crate::RunawayHalter`] side effect failed.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum HalterError {
    /// The override reason was empty. Every operator override must carry a
    /// justification that lands in the override receipt for post-hoc audit.
    #[error("override reason must not be empty")]
    EmptyOverrideReason,
    /// An override was attempted against a run that is not halted (it is either
    /// healthy, still in grace, or already killed — killed is terminal).
    #[error("run is not in a halted state; nothing to override")]
    NotHalted,
}
