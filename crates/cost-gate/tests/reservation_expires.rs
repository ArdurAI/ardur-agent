//! §11.14 Phase 1 — a reservation that lapses before finalize: reserve 500c,
//! advance the clock past the TTL, and assert finalize returns
//! `ReservationExpired`.

use std::sync::Arc;

use ardur_cost_gate::{
    AdmissionError, AdmissionRequest, CostAdmissionGate, CostEnvelope, CostTuple, HolderId,
    InMemoryBudgetStore, InMemoryCostAdmissionGate, ManualClock, ModelId, ProviderId, Sha256Digest,
    TokenId,
};
use uuid::Uuid;

#[tokio::test]
async fn finalize_after_expiry_is_reservation_expired() {
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

    // reserved_at = 0, expires_at = 1_000; jump well past it.
    clock.advance(2_000);

    let err = gate
        .finalize(reservation, CostTuple::cents(300))
        .await
        .expect_err("finalize must reject an expired reservation");
    assert!(matches!(err, AdmissionError::ReservationExpired));
}
