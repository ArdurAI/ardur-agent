//! Scenario §2.E3 — `cost_ceiling_exhaustion`.
//!
//! Drives the fused [`ChatRuntime`] against a tight budget with a provider that
//! bills a fixed cost per turn, and submits until the cost gate refuses:
//!
//! - The budget covers exactly two turns' worth of spend.
//! - Turn 1 and turn 2 admit, dispatch, mint a receipt, and *finalize* (so the
//!   budget falls by the actual cost, not the larger reserved envelope).
//! - Turn 3's admission is refused with [`RuntimeError::CostCeilingExceeded`],
//!   before the provider is reached.
//! - **No leftover reservation:** the refused turn reserved nothing, and both
//!   admitted turns finalized, so the remaining balance equals exactly
//!   `initial − 2 × per-turn-cost`. A stranded hold would show up as a *lower*
//!   balance.

use std::sync::Arc;

use ardur_e2e_tests::fixtures::{self};

use ardur_cost_gate::CostEnvelope;
use ardur_runtime::{
    CapTokenRef, ChatMessage, ChatRuntime, RuntimeError, SessionId, SubmitRequest,
};

mod support;
use support::BillingProvider;

/// Cents billed per turn.
const PER_TURN_CENTS: u64 = 100;
/// Starting budget: enough for exactly two turns, short of a third.
const BUDGET_CENTS: u64 = 250;

#[tokio::test]
async fn submitting_until_refused_exhausts_the_budget_without_leftover_holds() {
    let provider = Arc::new(BillingProvider::new(PER_TURN_CENTS));

    // A per-turn envelope that constrains only the cents axis, and a budget that
    // covers two turns but not three.
    let envelope = CostEnvelope {
        tokens_in_max: 0,
        tokens_out_max: 0,
        cents_max: PER_TURN_CENTS as u32,
        wall_ms_max: 0,
        attention_score_max: 0,
    };
    let runtime = fixtures::fused_builder(provider.clone())
        .projected_envelope(envelope)
        .provision_budget(
            fixtures::gate_holder(),
            ardur_cost_gate::CostTuple {
                tokens_in: 0,
                tokens_out: 0,
                cents: BUDGET_CENTS,
                wall_ms: 0,
                attention_score: 0,
            },
        )
        .build()
        .expect("the fused runtime wires");

    let token = fixtures::dev_valid_cap_token();
    let session_id = SessionId::new();
    let request = |content: &str| SubmitRequest {
        messages: vec![ChatMessage::user(content)],
        cap_token: CapTokenRef(token.clone()),
        session_id,
        requested_provider: None,
    };

    // Submit until the gate refuses.
    let mut successes = 0u64;
    let mut refusal: Option<RuntimeError> = None;
    for i in 0..10 {
        match runtime.submit(request(&format!("turn {i}"))).await {
            Ok(_) => successes += 1,
            Err(e) => {
                refusal = Some(e);
                break;
            }
        }
    }

    // Exactly two turns fit; the third was refused on cost grounds.
    assert_eq!(successes, 2, "the budget covered exactly two turns");
    assert!(
        matches!(refusal, Some(RuntimeError::CostCeilingExceeded)),
        "the over-budget turn is refused with CostCeilingExceeded, got {refusal:?}"
    );
    assert_eq!(
        provider.call_count(),
        2,
        "the refused turn never reached the provider"
    );

    // No leftover reservation: balance == initial − 2 × per-turn cost. A stranded
    // hold (un-finalized reservation) would leave the balance lower than this.
    let remaining = runtime
        .remaining_budget(&fixtures::gate_holder())
        .await
        .expect("the holder is provisioned");
    assert_eq!(
        remaining.cents,
        BUDGET_CENTS - 2 * PER_TURN_CENTS,
        "every admitted reservation finalized; none was stranded"
    );
}
