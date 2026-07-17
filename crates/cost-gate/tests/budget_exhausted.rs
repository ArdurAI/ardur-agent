//! §11.14 Phase 1 — a 500c request against a 100c budget is refused with
//! `BudgetExhausted { required: 500, available: 100 }`.

use ardur_cost_gate::{
    AdmissionError, AdmissionRequest, CostAdmissionGate, CostEnvelope, CostTuple, HolderId,
    InMemoryBudgetStore, InMemoryCostAdmissionGate, ModelId, ProviderId, Sha256Digest, TokenId,
};
use uuid::Uuid;

#[tokio::test]
async fn admit_over_balance_is_budget_exhausted() {
    let holder = HolderId("holder-1".into());
    let store = InMemoryBudgetStore::new();
    store.set_balance(holder.clone(), CostTuple::cents(100));

    let gate = InMemoryCostAdmissionGate::new(store);
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

    match gate.admit(req).await {
        Err(AdmissionError::BudgetExhausted {
            required,
            available,
        }) => {
            assert_eq!(required, 500);
            assert_eq!(available, 100);
        }
        other => panic!("expected BudgetExhausted, got {other:?}"),
    }
}
