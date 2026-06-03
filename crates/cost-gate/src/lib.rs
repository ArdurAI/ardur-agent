//! ardur-cost-gate — the cost-admission choke point every LLM call routes through.
//!
//! Plan family: §11.14
//! (`plans/11.14-cost-ceilings-receipts-cap-tokens-blueprint.md`). Design
//! record: ADR-Phase3-548 (CostAdmissionGate as a four-stage pipeline — every
//! call projects a cost envelope, is screened against ceilings, atomically
//! reserves that envelope against the holder's budget, and on completion
//! finalizes the actual cost and refunds the unspent delta). Reserving *before*
//! the call and refunding the difference *after* is what makes a budget a hard
//! ceiling rather than an after-the-fact accounting line: an in-flight call can
//! never overspend what was reserved, and concurrent calls cannot collectively
//! exceed the balance.
//!
//! # Phase 1 (this crate)
//!
//! - [`CostEnvelope`] / [`CostTuple`] / [`CostDelta`] — the projected ceiling, a
//!   holder's remaining budget, and the signed difference refunded at finalize.
//! - [`CostAdmissionGate`] / [`InMemoryCostAdmissionGate`] — [`CostAdmissionGate::admit`]
//!   runs stages 1–3 (project → check ceilings → reserve), returning a
//!   [`Reservation`]; [`CostAdmissionGate::finalize`] runs stage 4 (finalize +
//!   refund), returning a [`RefundReceipt`].
//! - [`BudgetStore`] / [`InMemoryBudgetStore`] — the holder-keyed budget ledger.
//!   The in-memory store does its check-and-decrement optimistically: each
//!   mutation bumps a `u64` version, and [`BudgetStore::try_reserve`] commits
//!   only if the version it read is still current, retrying on conflict.
//! - [`Clock`] / [`SystemClock`] / [`ManualClock`] — reservation expiry is
//!   wall-clock driven; [`ManualClock`] makes the expiry path deterministic in
//!   tests without sleeping.
//!
//! # Stage 2 is a Phase-1 stub
//!
//! ADR-Phase3-548 routes ceiling enforcement through a Cedar policy. Phase 1
//! stands in with an optional hard [`CostEnvelope`] ceiling
//! ([`AdmissionError::PolicyDenied`]) and an optional provider allowlist
//! ([`AdmissionError::ProviderNotAllowed`]); holder resolution from the
//! cap-token is a `bind_token` directory ([`AdmissionError::CapTokenInvalid`]
//! for an unknown token). See the inline `// TODO §11.14 Phase 2:` markers for
//! the Cedar policy evaluation, per-org ceilings, persistent budget backend,
//! and real cap-token (Biscuit) holder resolution that replace these.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod budget;
mod clock;
mod error;
mod gate;
mod types;

pub use budget::{BudgetStore, InMemoryBudgetStore};
pub use clock::{Clock, ManualClock, SystemClock};
pub use error::{AdmissionError, BudgetError, ProvisionError};
pub use gate::{CostAdmissionGate, InMemoryCostAdmissionGate};
pub use types::{
    AdmissionRequest, CostDelta, CostEnvelope, CostTuple, ModelId, ProviderId, RefundReceipt,
    Reservation, ReservationHandle, ReservationStatus, Sha256Digest, TokenId, UnixTsMillis,
};

/// The principal a budget is held against (a runtime profile, agent, org, or
/// session). Opaque string identifier — typically a SPIFFE-style URI. Mirrors
/// `ardur-cap-token`'s holder identity so a cap-token can later resolve to the
/// budget it spends against (Phase 2).
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct HolderId(pub String);
