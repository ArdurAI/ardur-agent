//! The crate's two typed-error surfaces: [`AdmissionError`] (the gate's verdict)
//! and [`BudgetError`] (the store's verdict). They are distinct because the
//! gate translates a store outcome into an admission verdict — e.g. a budget
//! the store could not claim becomes a [`AdmissionError::BudgetExhausted`] with
//! the request/available figures the store does not carry.

use crate::types::ProviderId;

/// Why the gate refused (or could not complete) an admission.
#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    /// Stage 3: the holder's budget cannot cover the projected envelope. Figures
    /// are the cents dimension — Ardur's canonical budget axis in Phase 1.
    #[error("budget exhausted: required {required}c, {available}c available")]
    BudgetExhausted {
        /// Cents the request projected.
        required: u32,
        /// Cents currently available to the holder.
        available: u32,
    },

    /// Stage 1: the cap-token did not resolve to a known holder. In Phase 1 this
    /// means the token was never `bind_token`'d; Phase 2 verifies the Biscuit.
    #[error("cap-token invalid or unknown")]
    CapTokenInvalid,

    /// Stage 2: a ceiling policy rejected the request. Phase 1 carries the
    /// hard-ceiling reason; Phase 2 carries the denying Cedar policy.
    #[error("policy denied: {0}")]
    PolicyDenied(String),

    /// [`finalize`](crate::CostAdmissionGate::finalize) was called after the
    /// reservation's expiry; the hold has already been released.
    #[error("reservation expired")]
    ReservationExpired,

    /// Stage 2: the request's provider is not in the gate's allowlist.
    #[error("provider not allowed: {0:?}")]
    ProviderNotAllowed(ProviderId),

    /// An unexpected internal failure (e.g. a store error that should not occur
    /// in normal operation).
    #[error("internal cost-gate error: {0}")]
    Internal(anyhow::Error),
}

/// Why a [`BudgetStore`](crate::BudgetStore) operation failed.
#[derive(Debug, thiserror::Error)]
pub enum BudgetError {
    /// No budget is provisioned for the holder.
    #[error("holder has no budget")]
    HolderNotFound,

    /// The requested envelope could not be atomically claimed: a concurrent
    /// reservation took the budget first (an optimistic-concurrency loss, or
    /// the remaining balance can no longer cover the request).
    #[error("lost the race to reserve the budget")]
    RaceLost,

    /// An unexpected internal failure.
    #[error("internal budget-store error: {0}")]
    Internal(anyhow::Error),
}
