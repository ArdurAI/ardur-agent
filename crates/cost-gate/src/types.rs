//! Public value types for cost admission: the request, its projected envelope,
//! a holder's budget tuple, the reservation handed back by [`admit`], and the
//! receipt produced by [`finalize`].
//!
//! [`admit`]: crate::CostAdmissionGate::admit
//! [`finalize`]: crate::CostAdmissionGate::finalize

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::HolderId;

// The cost tuple/envelope/delta, the id newtypes, and the request digest are
// owned by `ardur-core-types` and re-exported here so `ardur_cost_gate::{…}`
// paths — and this crate's own admission types — resolve to the one canonical
// type. `CostTuple::attention_score` is now a fixed-point milli-attention
// integer shared with the runtime and receipt layers, so a completed turn's
// attention flows into the budget without the old lossy `f64 as u64` cast.
pub use ardur_core_types::{
    CostDelta, CostEnvelope, CostTuple, ModelId, ProviderId, Sha256Digest, TokenId,
};

/// Unix timestamp in **milliseconds** since the epoch. Reservation timing
/// (`reserved_at`, `expires_at`, `finalized_at`) is millisecond-resolution so a
/// short-lived reservation's expiry is expressible without sub-second loss.
// NOTE §0.0 reconciliation: `ardur-core-types` owns a `UnixTsMillis(u64)`
// newtype (the form `ardur-receipt` and `ardur-memory` use). The cost gate
// keeps a bare `u64` alias here because its reservation-expiry arithmetic
// operates on the raw millis; adopting the newtype is a mechanical follow-up
// that does not change the wire form (the newtype is `#[serde(transparent)]`).
pub type UnixTsMillis = u64;

/// A request to admit a single metered call. Carries the cap-token it spends
/// against, the projected [`CostEnvelope`], the provider/model it targets, and
/// a [`Sha256Digest`] of the request body so the resulting reservation is bound
/// to exactly this call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionRequest {
    /// The cap-token whose budget this call spends.
    pub cap_token_id: TokenId,
    /// The projected ceiling for this call.
    pub projected_envelope: CostEnvelope,
    /// The provider this call targets.
    pub provider_id: ProviderId,
    /// The model this call targets.
    pub model_id: ModelId,
    /// SHA-256 of the request body, binding the reservation to this request.
    pub request_digest: Sha256Digest,
}

/// Where a [`Reservation`] is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReservationStatus {
    /// Reserved and in flight — awaiting finalize.
    Active,
    /// Finalized: actual cost posted and the unspent delta refunded.
    Finalized,
    /// Expired before finalize; the hold was released back to the budget.
    Expired,
    /// Cancelled before finalize; the hold was released back to the budget.
    Cancelled,
}

/// The receipt [`admit`](crate::CostAdmissionGate::admit) hands back: an
/// envelope is now held against the holder's budget until [`finalize`] or
/// expiry. The caller presents this back to finalize.
///
/// [`finalize`]: crate::CostAdmissionGate::finalize
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    /// Stable per-reservation id (UUIDv4).
    pub reservation_id: Uuid,
    /// The cap-token this reservation spends against.
    pub cap_token_id: TokenId,
    /// The envelope held against the budget.
    pub envelope: CostEnvelope,
    /// When the reservation was taken.
    pub reserved_at: UnixTsMillis,
    /// When the hold lapses if not finalized first.
    pub expires_at: UnixTsMillis,
    /// Lifecycle status at the time this value was produced.
    pub status: ReservationStatus,
}

/// The receipt [`finalize`](crate::CostAdmissionGate::finalize) produces: the
/// actual cost posted and the signed delta refunded back to the holder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefundReceipt {
    /// The reservation this finalizes.
    pub reservation_id: Uuid,
    /// The actual cost the call incurred.
    pub actual: CostTuple,
    /// The signed `reserved - actual` delta credited (or debited) to the holder.
    pub refunded: CostDelta,
    /// When the finalize was posted.
    pub finalized_at: UnixTsMillis,
}

/// An opaque handle returned by [`try_reserve`](crate::BudgetStore::try_reserve)
/// and consumed by [`refund`](crate::BudgetStore::refund). It carries exactly
/// what the store needs to credit the right holder back: who was charged, how
/// much, and the store version at which the charge committed.
#[derive(Clone, Debug)]
pub struct ReservationHandle {
    /// The holder whose budget was decremented.
    pub holder: HolderId,
    /// The absolute amount decremented (the envelope, widened).
    pub reserved: CostTuple,
    /// The store version produced by the committing decrement.
    pub committed_version: u64,
}
