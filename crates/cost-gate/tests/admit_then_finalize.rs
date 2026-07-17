//! §11.14 Phase 1 — the happy path: admit a 500c call against a 1000c budget,
//! finalize at 300c actual, and assert the refund is the 200c unspent delta.

use ardur_cost_gate::{
    AdmissionRequest, CostAdmissionGate, CostEnvelope, CostTuple, HolderId, InMemoryBudgetStore,
    InMemoryCostAdmissionGate, ModelId, ProviderId, Sha256Digest, TokenId,
};
use uuid::Uuid;

fn request(token: TokenId, cents: u32) -> AdmissionRequest {
    AdmissionRequest {
        cap_token_id: token,
        projected_envelope: CostEnvelope {
            cents_max: cents,
            ..Default::default()
        },
        provider_id: ProviderId("anthropic".into()),
        model_id: ModelId("claude".into()),
        request_digest: Sha256Digest::of(b"request-body"),
    }
}

#[tokio::test]
async fn admit_then_finalize_refunds_the_unspent_delta() {
    let holder = HolderId("holder-1".into());
    let store = InMemoryBudgetStore::new();
    store.set_balance(holder.clone(), CostTuple::cents(1000));

    let gate = InMemoryCostAdmissionGate::new(store);
    let token = TokenId(Uuid::new_v4());
    gate.bind_token(token, holder);

    let reservation = gate.admit(request(token, 500)).await.expect("admit");

    let receipt = gate
        .finalize(reservation, CostTuple::cents(300))
        .await
        .expect("finalize");

    // reserved 500c - actual 300c = 200c refunded back to the holder.
    assert_eq!(receipt.refunded.cents, 200);
    assert_eq!(receipt.actual.cents, 300);
}
