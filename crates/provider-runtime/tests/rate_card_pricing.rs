//! §3.0 Phase 1 — the Anthropic rate card prices token usage into whole cents.

use ardur_provider_runtime::{RateCard, Usage};

#[test]
fn prices_input_and_output_tokens() {
    let card = RateCard::anthropic_2026_q2_v1();

    // 0.3¢/1k input, 1.5¢/1k output → 10k + 10k = 3¢ + 15¢ = 18¢.
    let cost = card.price(Usage {
        tokens_in: 10_000,
        tokens_out: 10_000,

        ..Default::default()
    });

    assert_eq!(cost.tokens_in, 10_000);
    assert_eq!(cost.tokens_out, 10_000);
    assert_eq!(cost.cents, 18);
    assert_eq!(
        cost.wall_ms, 0,
        "the provider does not fill wall-clock time"
    );
}

#[test]
fn zero_usage_is_free() {
    let cost = RateCard::anthropic_2026_q2_v1().price(Usage::default());
    assert_eq!(cost.cents, 0);
}

#[test]
fn rounds_to_whole_cents() {
    // 1k in + 1k out = 0.3¢ + 1.5¢ = 1.8¢, which rounds to 2¢.
    let cost = RateCard::anthropic_2026_q2_v1().price(Usage {
        tokens_in: 1_000,
        tokens_out: 1_000,

        ..Default::default()
    });
    assert_eq!(cost.cents, 2);
}

#[test]
fn sub_cent_cost_rounds_up_to_one_cent_ard_495() {
    // 1k input only @ 0.3¢/1k = 0.3¢ → must charge 1¢, never 0 (a 0¢ turn
    // would be admitted, executed, and finalized free — a budget bypass).
    let cost = RateCard::anthropic_2026_q2_v1().price(Usage {
        tokens_in: 1_000,
        tokens_out: 0,
        ..Default::default()
    });
    assert_eq!(cost.cents, 1, "sub-cent cost must not round to 0 (ARD-495)");
}
