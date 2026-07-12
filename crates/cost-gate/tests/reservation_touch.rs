//! ARD-501 — a reservation that is actively refreshed survives past its
//! original TTL. Reserve 500c, advance almost to expiry, `touch_reservation`,
//! advance again, and assert finalize still settles (does not discard the turn).

use std::sync::Arc;

use ardur_cost_gate::{
    AdmissionRequest, CostAdmissionGate, CostEnvelope, CostTuple, HolderId, InMemoryBudgetStore,
    InMemoryCostAdmissionGate, ManualClock, ModelId, ProviderId, Sha256Digest, TokenId,
};
use uuid::Uuid;

#[tokio::test]
async fn touched_reservation_survives_past_original_ttl() {
    let holder = HolderId("holder-1".into());
    let store = InMemoryBudgetStore::new();
    store.set_balance(holder.clone(), CostTuple::cents(1000));

    let clock = Arc::new(ManualClock::new(0));
    let gate = InMemoryCostAdmissionGate::with_clock(store, clock.clone()).with_ttl_ms(1_000);
    let token = TokenId(Uuid::new_v4());
    gate.bind_token(token, holder);

    let req = AdmissionRequest {
        cap_token_id: token,
        projected_envelope: CostEnvelope {
            cents_max: 500,
            ..Default::default()
        },
        provider_id: ProviderId("anthropic".into()),
        model_id: ModelId("claude".into()),
        request_digest: Sha256Digest::of(b"request-body"),
    };

    let reservation = gate.admit(req).await.expect("admit");

    // reserved_at = 0, expires_at = 1_000. Stream for a while, refreshing the
    // lease each "chunk", then run past the *original* deadline.
    clock.advance(900);
    assert!(
        gate.touch_reservation(reservation.reservation_id),
        "an active reservation is refreshed"
    );
    clock.advance(900); // now = 1_800 > original 1_000, but < refreshed 1_900.

    let receipt = gate
        .finalize(reservation, CostTuple::cents(300))
        .await
        .expect("a refreshed reservation still finalizes");
    assert_eq!(receipt.actual, CostTuple::cents(300));
}

#[tokio::test]
async fn touch_is_a_noop_for_a_finalized_reservation() {
    let holder = HolderId("holder-1".into());
    let store = InMemoryBudgetStore::new();
    store.set_balance(holder.clone(), CostTuple::cents(1000));

    let clock = Arc::new(ManualClock::new(0));
    let gate = InMemoryCostAdmissionGate::with_clock(store, clock.clone()).with_ttl_ms(1_000);
    let token = TokenId(Uuid::new_v4());
    gate.bind_token(token, holder);

    let req = AdmissionRequest {
        cap_token_id: token,
        projected_envelope: CostEnvelope {
            cents_max: 500,
            ..Default::default()
        },
        provider_id: ProviderId("anthropic".into()),
        model_id: ModelId("claude".into()),
        request_digest: Sha256Digest::of(b"request-body"),
    };

    let reservation = gate.admit(req).await.expect("admit");
    let reservation_id = reservation.reservation_id;
    gate.finalize(reservation, CostTuple::cents(300))
        .await
        .expect("finalize");

    assert!(
        !gate.touch_reservation(reservation_id),
        "a finalized reservation cannot be refreshed"
    );
}
