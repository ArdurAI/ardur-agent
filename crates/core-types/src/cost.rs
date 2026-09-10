//! The "cost as protocol primitive" trio: [`CostTuple`], [`CostEnvelope`], and
//! [`CostDelta`].
//!
//! # `attention_score` is fixed-point, not floating-point
//!
//! `ardur-runtime` and `ardur-receipt` historically carried `attention_score`
//! as an `f64` in `0.0..=1.0`, while `ardur-cost-gate` carried it as a `u64`
//! budget axis. The fused runtime bridged the two with `score as u64`, which
//! floored every fractional score to `0` — the attention budget was silently
//! dead, and a float field also cost the tuple its `Eq` and made receipt bytes
//! depend on float formatting.
//!
//! The reconciliation makes `attention_score` a fixed-point integer measured in
//! **milli-attention** (thousandths of one unit of human attention), so the old
//! `0.0..=1.0` share maps onto `0..=1000` losslessly — see
//! [`MILLI_ATTENTION_PER_UNIT`]. One integer type now flows runtime → cost-gate
//! → receipt with no lossy cast, exact ledger arithmetic, `Eq`, and byte-stable
//! receipts.

use serde::{Deserialize, Serialize};

/// Milli-attention units per one whole unit of human attention. A legacy
/// `0.0..=1.0` attention share `s` is `(s * MILLI_ATTENTION_PER_UNIT as f64)`
/// milli-attention; e.g. `0.5` → `500`.
pub const MILLI_ATTENTION_PER_UNIT: u64 = 1_000;

/// The cost a single metered action incurred, and the shape a holder's budget
/// is held in. The D-4 cost tuple: token counts, monetary cost in whole cents,
/// wall-clock duration, and attention consumed.
///
/// Every axis is `u64` so accumulated budgets do not overflow and the tuple is
/// `Eq` — cost values are compared and hash-chained into receipts, so exact
/// equality matters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CostTuple {
    /// Prompt/input tokens billed.
    pub tokens_in: u64,
    /// Completion/output tokens billed.
    pub tokens_out: u64,
    /// Monetary cost in whole US cents.
    pub cents: u64,
    /// Wall-clock duration of the action, in milliseconds.
    pub wall_ms: u64,
    /// Human attention consumed, in **milli-attention** units (thousandths of
    /// one unit; see [`MILLI_ATTENTION_PER_UNIT`]). Integer so the attention
    /// budget decrements exactly and receipt bytes are stable.
    ///
    /// Deserialization is migration-tolerant: a receipt persisted by an older
    /// build carries this axis as a `0.0..=1.0` float, which is coerced to
    /// milli-attention rather than rejected — see [`de_attention_score`].
    #[serde(deserialize_with = "de_attention_score")]
    pub attention_score: u64,
}

/// Deserialize [`CostTuple::attention_score`], migrating the legacy on-disk
/// representation forward.
///
/// Older builds carried this axis as an `f64` attention *share* in `0.0..=1.0`
/// (see the module docs); it is now a milli-attention `u64`. A receipt written
/// by such a build therefore stores e.g. `"attention_score":0.0`, which a plain
/// `u64` field rejects with `invalid type: floating point, expected u64`. On the
/// boot-time receipt-chain reconciliation path that single line aborts the whole
/// load and bricks CLI and server startup for that data dir (issue #350).
///
/// Accept either shape: an integer is already milli-attention and passes through
/// unchanged; a float is a legacy share and is mapped losslessly onto `0..=1000`
/// (`share * MILLI_ATTENTION_PER_UNIT`, rounded). The migration is
/// *representational only* — a persisted receipt's JWS signature and hash-chain
/// linkage are verified over its original on-disk bytes, never over this decoded
/// value, so coercing the float weakens no trust check.
fn de_attention_score<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct AttentionScoreVisitor;

    impl<'de> serde::de::Visitor<'de> for AttentionScoreVisitor {
        type Value = u64;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a milli-attention integer or a legacy 0.0..=1.0 attention-share float")
        }

        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<u64, E> {
            Ok(v)
        }

        fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<u64, E> {
            u64::try_from(v).map_err(|_| E::custom(format!("attention_score out of range: {v}")))
        }

        fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<u64, E> {
            if !v.is_finite() || v < 0.0 {
                return Err(E::custom(format!("invalid legacy attention_score: {v}")));
            }
            // Legacy `0.0..=1.0` share → milli-attention (`0..=1000`).
            Ok((v * MILLI_ATTENTION_PER_UNIT as f64).round() as u64)
        }
    }

    deserializer.deserialize_any(AttentionScoreVisitor)
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

    /// Widen a [`CostEnvelope`]'s `u32` maxima into an absolute `u64` tuple —
    /// the amount a reservation decrements.
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
    /// `u64::MAX` rather than wrapping, so a degenerate top-up can never
    /// silently zero a balance.
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
    /// Maximum attention consumed, in milli-attention units (see
    /// [`CostTuple::attention_score`]).
    pub attention_score_max: u32,
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
    /// Attention-score (milli-attention) delta.
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

#[cfg(test)]
mod attention_regression {
    use super::*;

    // C1 (DEEP-CODE-REVIEW-2026-07-12): before consolidation the runtime/receipt
    // attention was an `f64` in 0.0..=1.0 and the fused runtime widened it into
    // the cost gate with `score as u64`, flooring every fractional value to 0 —
    // the attention budget was silently dead. With a milli-attention integer the
    // half-unit is 500 and it decrements the budget exactly.
    #[test]
    fn half_a_unit_of_attention_is_not_lost() {
        let turn = CostTuple {
            attention_score: MILLI_ATTENTION_PER_UNIT / 2,
            ..CostTuple::ZERO
        };
        assert_eq!(turn.attention_score, 500);

        // It decrements a budget exactly, rather than flooring to zero.
        let budget = CostTuple {
            attention_score: MILLI_ATTENTION_PER_UNIT,
            ..CostTuple::ZERO
        };
        assert!(budget.covers(&turn));
        let remaining = budget.checked_sub(&turn).expect("covered");
        assert_eq!(remaining.attention_score, 500);
    }

    // Byte-stable receipts: the tuple serializes as integers, so a receipt's
    // bytes (and therefore its signature and chain hash) do not depend on float
    // formatting, and it round-trips exactly.
    #[test]
    fn serde_round_trips_as_integers() {
        let cost = CostTuple {
            tokens_in: 100,
            tokens_out: 50,
            cents: 2,
            wall_ms: 1_200,
            attention_score: 750,
        };
        let json = serde_json::to_string(&cost).expect("serialize");
        assert!(
            json.contains("\"attention_score\":750"),
            "attention must serialize as an integer, got {json}"
        );
        let back: CostTuple = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(cost, back);
    }

    // Issue #350: a receipt persisted by an older build carries `attention_score`
    // as a `0.0..=1.0` float. Boot-time chain reconciliation must load it rather
    // than aborting, so the field deserializer coerces the legacy share onto the
    // milli-attention integer axis (`share * 1000`, rounded) instead of failing
    // with `invalid type: floating point, expected u64`.
    #[test]
    fn legacy_float_attention_score_migrates_to_milli() {
        for (json, expected) in [
            (
                r#"{"tokens_in":0,"tokens_out":0,"cents":0,"wall_ms":0,"attention_score":0.0}"#,
                0,
            ),
            (
                r#"{"tokens_in":0,"tokens_out":0,"cents":0,"wall_ms":0,"attention_score":0.5}"#,
                500,
            ),
            (
                r#"{"tokens_in":0,"tokens_out":0,"cents":0,"wall_ms":0,"attention_score":1.0}"#,
                1_000,
            ),
        ] {
            let cost: CostTuple = serde_json::from_str(json).expect("legacy float must migrate");
            assert_eq!(
                cost.attention_score, expected,
                "legacy share in {json} should map to {expected} milli-attention"
            );
        }
    }

    // The new integer representation is unchanged by the migration path: a
    // milli-attention integer passes through as-is (no accidental ×1000).
    #[test]
    fn integer_attention_score_passes_through_unchanged() {
        let json = r#"{"tokens_in":0,"tokens_out":0,"cents":0,"wall_ms":0,"attention_score":750}"#;
        let cost: CostTuple = serde_json::from_str(json).expect("deserialize");
        assert_eq!(cost.attention_score, 750);
    }

    // A genuinely malformed value (negative / non-finite share) is still an
    // error — tolerance is scoped to the legacy float shape, not "accept
    // anything".
    #[test]
    fn negative_attention_score_is_rejected() {
        let json = r#"{"tokens_in":0,"tokens_out":0,"cents":0,"wall_ms":0,"attention_score":-1.0}"#;
        assert!(serde_json::from_str::<CostTuple>(json).is_err());
    }
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
