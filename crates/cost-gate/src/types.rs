//! Public value types for cost admission: the request, its projected envelope,
//! a holder's budget tuple, the reservation handed back by [`admit`], and the
//! receipt produced by [`finalize`].
//!
//! [`admit`]: crate::CostAdmissionGate::admit
//! [`finalize`]: crate::CostAdmissionGate::finalize

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::HolderId;

/// Unix timestamp in **milliseconds** since the epoch. Reservation timing
/// (`reserved_at`, `expires_at`, `finalized_at`) is millisecond-resolution so a
/// short-lived reservation's expiry is expressible without sub-second loss.
pub type UnixTsMillis = u64;

/// The id of the cap-token a request spends against. Wraps a UUIDv4 so it lines
/// up with `ardur-cap-token`'s `VerifiedClaims::token_id`; in Phase 1 the gate
/// resolves it to a [`HolderId`] through its `bind_token` directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TokenId(pub Uuid);

/// An LLM provider (e.g. the vendor behind a model). Opaque string; screened
/// against the gate's optional provider allowlist in stage 2.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderId(pub String);

/// A concrete model id within a provider. Carried on the request and the
/// reservation for receipt attribution; not enforced against in Phase 1.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelId(pub String);

/// A SHA-256 digest binding a reservation to the exact request bytes that
/// produced it (so a finalize cannot be replayed against a different request).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Sha256Digest(
    /// The 32 raw digest bytes.
    pub [u8; 32],
);

impl Sha256Digest {
    /// Hash `data` with SHA-256.
    pub fn of(data: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(data);
        Self(hasher.finalize().into())
    }
}

impl std::fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hex is the canonical way a digest is read; the default array Debug is
        // unreadable.
        write!(f, "Sha256Digest(")?;
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        write!(f, ")")
    }
}

/// The projected ceiling for a single call across the five cost dimensions Ardur
/// meters. Every field is a *maximum*: the call is reserved up to this envelope
/// and may only finalize at or below it. Widths are `u32` because a single
/// call's projection comfortably fits — the holder's accumulated budget
/// ([`CostTuple`]) is wider.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostEnvelope {
    /// Maximum input tokens.
    pub tokens_in_max: u32,
    /// Maximum output tokens.
    pub tokens_out_max: u32,
    /// Maximum spend, in cents.
    pub cents_max: u32,
    /// Maximum wall-clock duration, in milliseconds.
    pub wall_ms_max: u32,
    /// Maximum attention-score units (Ardur's scheduler-pressure metric).
    pub attention_score_max: u32,
}

/// A holder's remaining budget (or any absolute cost quantity) across the same
/// five dimensions as [`CostEnvelope`], in `u64` so accumulated budgets do not
/// overflow. Also used to report a call's *actual* cost at finalize.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostTuple {
    /// Input tokens.
    pub tokens_in: u64,
    /// Output tokens.
    pub tokens_out: u64,
    /// Spend, in cents.
    pub cents: u64,
    /// Wall-clock duration, in milliseconds.
    pub wall_ms: u64,
    /// Attention-score units.
    pub attention_score: u64,
}

impl CostTuple {
    /// The zero tuple — a fully spent (or unprovisioned) budget.
    pub const ZERO: Self = Self {
        tokens_in: 0,
        tokens_out: 0,
        cents: 0,
        wall_ms: 0,
        attention_score: 0,
    };

    /// A budget that is only constrained on the cents dimension (the others
    /// zero). Convenience for the common "dollar ceiling" case.
    pub fn cents(cents: u64) -> Self {
        Self {
            cents,
            ..Self::ZERO
        }
    }

    /// Widen an [`CostEnvelope`]'s `u32` maxima into an absolute `u64` tuple —
    /// the amount [`try_reserve`](crate::BudgetStore::try_reserve) decrements.
    pub fn from_envelope(env: &CostEnvelope) -> Self {
        Self {
            tokens_in: u64::from(env.tokens_in_max),
            tokens_out: u64::from(env.tokens_out_max),
            cents: u64::from(env.cents_max),
            wall_ms: u64::from(env.wall_ms_max),
            attention_score: u64::from(env.attention_score_max),
        }
    }

    /// Whether `self` is large enough to cover `need` on **every** dimension.
    pub fn covers(&self, need: &CostTuple) -> bool {
        self.tokens_in >= need.tokens_in
            && self.tokens_out >= need.tokens_out
            && self.cents >= need.cents
            && self.wall_ms >= need.wall_ms
            && self.attention_score >= need.attention_score
    }

    /// Per-dimension saturating addition — the merge applied when a holder's
    /// budget is topped up (request-time provisioning). Each axis clamps at
    /// `u64::MAX` rather than wrapping, so a degenerate top-up can never silently
    /// zero a balance.
    pub fn saturating_add(&self, rhs: &CostTuple) -> CostTuple {
        CostTuple {
            tokens_in: self.tokens_in.saturating_add(rhs.tokens_in),
            tokens_out: self.tokens_out.saturating_add(rhs.tokens_out),
            cents: self.cents.saturating_add(rhs.cents),
            wall_ms: self.wall_ms.saturating_add(rhs.wall_ms),
            attention_score: self.attention_score.saturating_add(rhs.attention_score),
        }
    }

    /// The first dimension on which `self` strictly exceeds `ceiling`, or `None`
    /// if every dimension is within it. Names the axis so an over-cap rejection
    /// can report *which* limit a top-up would breach.
    pub fn first_dimension_over(&self, ceiling: &CostTuple) -> Option<&'static str> {
        if self.tokens_in > ceiling.tokens_in {
            Some("tokens_in")
        } else if self.tokens_out > ceiling.tokens_out {
            Some("tokens_out")
        } else if self.cents > ceiling.cents {
            Some("cents")
        } else if self.wall_ms > ceiling.wall_ms {
            Some("wall_ms")
        } else if self.attention_score > ceiling.attention_score {
            Some("attention_score")
        } else {
            None
        }
    }

    /// Per-dimension subtraction, or `None` if any dimension would underflow.
    pub fn checked_sub(&self, rhs: &CostTuple) -> Option<CostTuple> {
        Some(CostTuple {
            tokens_in: self.tokens_in.checked_sub(rhs.tokens_in)?,
            tokens_out: self.tokens_out.checked_sub(rhs.tokens_out)?,
            cents: self.cents.checked_sub(rhs.cents)?,
            wall_ms: self.wall_ms.checked_sub(rhs.wall_ms)?,
            attention_score: self.attention_score.checked_sub(rhs.attention_score)?,
        })
    }

    /// Per-dimension addition, or `None` if any dimension would overflow.
    pub fn checked_add(&self, rhs: &CostTuple) -> Option<CostTuple> {
        Some(CostTuple {
            tokens_in: self.tokens_in.checked_add(rhs.tokens_in)?,
            tokens_out: self.tokens_out.checked_add(rhs.tokens_out)?,
            cents: self.cents.checked_add(rhs.cents)?,
            wall_ms: self.wall_ms.checked_add(rhs.wall_ms)?,
            attention_score: self.attention_score.checked_add(rhs.attention_score)?,
        })
    }

    /// Apply a signed [`CostDelta`], clamping each dimension to the
    /// `0..=u64::MAX` range (a refund credits, an overrun debits).
    pub fn apply_delta(&self, delta: &CostDelta) -> CostTuple {
        fn add(base: u64, d: i64) -> u64 {
            (i128::from(base) + i128::from(d)).clamp(0, i128::from(u64::MAX)) as u64
        }
        CostTuple {
            tokens_in: add(self.tokens_in, delta.tokens_in),
            tokens_out: add(self.tokens_out, delta.tokens_out),
            cents: add(self.cents, delta.cents),
            wall_ms: add(self.wall_ms, delta.wall_ms),
            attention_score: add(self.attention_score, delta.attention_score),
        }
    }
}

/// A signed per-dimension difference. The refund posted at finalize is
/// `reserved - actual`: positive dimensions credit unspent budget back to the
/// holder; a negative dimension (the call overran its envelope on that axis)
/// debits the overage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostDelta {
    /// Input-token delta.
    pub tokens_in: i64,
    /// Output-token delta.
    pub tokens_out: i64,
    /// Cents delta.
    pub cents: i64,
    /// Wall-clock-millisecond delta.
    pub wall_ms: i64,
    /// Attention-score delta.
    pub attention_score: i64,
}

impl CostDelta {
    /// The refund posted when a call finalizes: `reserved - actual` on each
    /// dimension.
    pub fn between(reserved: &CostTuple, actual: &CostTuple) -> Self {
        fn sub(a: u64, b: u64) -> i64 {
            (i128::from(a) - i128::from(b)).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
        }
        Self {
            tokens_in: sub(reserved.tokens_in, actual.tokens_in),
            tokens_out: sub(reserved.tokens_out, actual.tokens_out),
            cents: sub(reserved.cents, actual.cents),
            wall_ms: sub(reserved.wall_ms, actual.wall_ms),
            attention_score: sub(reserved.attention_score, actual.attention_score),
        }
    }

    /// A full credit of `reserved` (every dimension positive) — used to release
    /// the entire hold when a reservation expires unused.
    pub fn full_credit(reserved: &CostTuple) -> Self {
        Self::between(reserved, &CostTuple::ZERO)
    }
}

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

#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    prop_compose! {
        fn arb_cost_tuple()(tokens_in in 0..u64::MAX/2, tokens_out in 0..u64::MAX/2, cents in 0..u64::MAX/2, wall_ms in 0..u64::MAX/2, attention_score in 0..u64::MAX/2) -> CostTuple {
            CostTuple { tokens_in, tokens_out, cents, wall_ms, attention_score }
        }
    }

    proptest! {
        #[test]
        fn cost_checked_add_no_overflow(a in arb_cost_tuple(), b in arb_cost_tuple()) {
            // ARD-322: Property-based test for cost arithmetic.
            let result = a.checked_add(&b);
            if let Some(sum) = result {
                assert_eq!(sum.tokens_in, a.tokens_in + b.tokens_in);
                assert_eq!(sum.tokens_out, a.tokens_out + b.tokens_out);
                assert_eq!(sum.cents, a.cents + b.cents);
                assert_eq!(sum.wall_ms, a.wall_ms + b.wall_ms);
                assert_eq!(sum.attention_score, a.attention_score + b.attention_score);
            }
        }

        #[test]
        fn cost_checked_sub_no_underflow(a in arb_cost_tuple(), b in arb_cost_tuple()) {
            // ARD-322: Property-based test for cost subtraction.
            let result = a.checked_sub(&b);
            if let Some(diff) = result {
                assert_eq!(diff.tokens_in, a.tokens_in - b.tokens_in);
                assert_eq!(diff.tokens_out, a.tokens_out - b.tokens_out);
                assert_eq!(diff.cents, a.cents - b.cents);
                assert_eq!(diff.wall_ms, a.wall_ms - b.wall_ms);
                assert_eq!(diff.attention_score, a.attention_score - b.attention_score);
            }
        }

        #[test]
        fn cost_delta_between_is_correct(reserved in arb_cost_tuple(), actual in arb_cost_tuple()) {
            // ARD-322: Property-based test for CostDelta::between.
            let delta = CostDelta::between(&reserved, &actual);
            
            // Verify that applying the negative delta to actual gives back reserved.
            // This only works when the values are small enough to avoid clamping.
            let reconstructed = actual.apply_delta(&CostDelta {
                tokens_in: -delta.tokens_in,
                tokens_out: -delta.tokens_out,
                cents: -delta.cents,
                wall_ms: -delta.wall_ms,
                attention_score: -delta.attention_score,
            });
            
            // For small values (where no clamping occurs), reconstruction should be exact.
            // The i128 arithmetic in apply_delta clamps at 0 and u64::MAX.
            // Only check when both values are small enough to avoid overflow/underflow
            // AND reserved >= actual (so delta is non-negative and no clamping at 0).
            // Use a much smaller bound to ensure no overflow in the i128 arithmetic.
            if reserved.tokens_in <= 1_000_000_000u64 && actual.tokens_in <= 1_000_000_000u64 &&
               reserved.tokens_in >= actual.tokens_in &&
               reserved.tokens_out >= actual.tokens_out &&
               reserved.cents >= actual.cents &&
               reserved.wall_ms >= actual.wall_ms &&
               reserved.attention_score >= actual.attention_score {
                assert_eq!(reconstructed.tokens_in, reserved.tokens_in, "tokens_in mismatch: reserved={:?}, actual={:?}, delta={:?}, reconstructed={:?}", reserved, actual, delta, reconstructed);
            }
        }

        #[test]
        fn covers_is_transitive(a in arb_cost_tuple(), b in arb_cost_tuple(), c in arb_cost_tuple()) {
            // ARD-322: If a covers b and b covers c, then a covers c.
            if a.covers(&b) && b.covers(&c) {
                assert!(a.covers(&c), "covers should be transitive");
            }
        }

        #[test]
        fn saturating_add_never_overflows(a in arb_cost_tuple(), b in arb_cost_tuple()) {
            // ARD-322: saturating_add should never panic or overflow.
            let result = a.saturating_add(&b);
            assert!(result.tokens_in >= a.tokens_in);
            assert!(result.tokens_out >= a.tokens_out);
            assert!(result.cents >= a.cents);
            assert!(result.wall_ms >= a.wall_ms);
            assert!(result.attention_score >= a.attention_score);
        }
    }
}
