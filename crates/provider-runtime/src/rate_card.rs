//! Versioned provider pricing — the table that turns a [`Usage`] into a billed
//! [`CostTuple`](ardur_runtime::CostTuple).

use ardur_runtime::CostTuple;
use serde::{Deserialize, Serialize};

use crate::types::Usage;

/// A frozen, versioned pricing table for one provider/model tier.
///
/// `version_id` pins the exact prices a cost was computed under, so a stored
/// receipt stays auditable even after the provider re-prices.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RateCard {
    /// Stable id of this pricing version (e.g. `"anthropic-2026-q2-v1"`).
    pub version_id: String,
    /// Price per 1,000 input tokens, in US cents.
    pub cents_per_1k_input: f64,
    /// Price per 1,000 output tokens, in US cents.
    pub cents_per_1k_output: f64,
    /// Flat per-request surcharge, in US cents.
    pub cents_per_request: f64,
}

impl RateCard {
    /// The Anthropic Q2-2026 pricing table (version `anthropic-2026-q2-v1`).
    ///
    /// Phase 1 ships representative per-1k rates; the authoritative table lands
    /// with the live HTTP path in Phase 2.
    #[must_use]
    pub fn anthropic_2026_q2_v1() -> Self {
        Self {
            version_id: "anthropic-2026-q2-v1".to_string(),
            cents_per_1k_input: 0.3,
            cents_per_1k_output: 1.5,
            cents_per_request: 0.0,
        }
    }

    /// Price `usage` under this card, rounded to whole US cents.
    ///
    /// The returned [`CostTuple`](ardur_runtime::CostTuple) carries the token
    /// counts and `cents`; `wall_ms` and `attention_score` stay zero — those are
    /// the runtime's to fill, not the provider's.
    ///
    /// When `usage.cost_cents` is `Some` (e.g. OpenRouter reports the actual
    /// dollar cost on the response), that value is used directly and the rate
    /// card is bypassed. This ensures receipted/gated costs match the provider's
    /// billed amount rather than an estimate.
    #[must_use]
    pub fn price(&self, usage: Usage) -> CostTuple {
        let cents = match usage.cost_cents {
            Some(actual) => actual,
            None => {
                let computed = self.cents_per_1k_input * f64::from(usage.tokens_in) / 1000.0
                    + self.cents_per_1k_output * f64::from(usage.tokens_out) / 1000.0
                    + self.cents_per_request;
                // ARD-495: round UP and never to zero for a positive cost, so a
                // sub-cent turn still depletes the budget by at least 1 cent.
                if computed <= 0.0 {
                    0
                } else {
                    (computed.ceil() as u64).max(1)
                }
            }
        };
        CostTuple {
            tokens_in: u64::from(usage.tokens_in),
            tokens_out: u64::from(usage.tokens_out),
            cents,
            wall_ms: 0,
            attention_score: 0.0,
        }
    }
}
