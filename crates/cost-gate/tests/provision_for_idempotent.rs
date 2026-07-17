//! §11.14 / ARD-49 — request-time provisioning ([`CostAdmissionGate::provision_for`]).
//!
//! Proves the documented **additive merge** policy: provisioning the same
//! subject twice accumulates (the balance is the *sum*, not the last write).
//! This is the property a per-turn top-up server relies on — a second top-up
//! must add to unspent budget rather than overwrite it.
//!
//! Two angles:
//! - the store primitive [`BudgetStore::provision_merge`] returns the merged
//!   balance, so the sum is asserted directly and to the cent;
//! - the gate's [`CostAdmissionGate::provision_for`] is proven *through its
//!   public surface*: two 100c top-ups let a 150c admission succeed that a
//!   single top-up could not — the balance must have summed to 200c.
//!
//! A third scenario proves the optional per-subject cap refuses an over-cap
//! merge and leaves the balance untouched.

use ardur_cost_gate::{
    AdmissionError, AdmissionRequest, BudgetStore, CostAdmissionGate, CostEnvelope, CostTuple,
    HolderId, InMemoryBudgetStore, InMemoryCostAdmissionGate, ModelId, ProviderId, ProvisionError,
    Sha256Digest, TokenId,
};
use uuid::Uuid;

#[tokio::test]
async fn provision_merge_sums_the_balance_and_returns_it() {
    let subject = HolderId("holder-merge".into());
    let store = InMemoryBudgetStore::new();

    // The subject starts unprovisioned: the first merge creates the account.
    let after_first = store
        .provision_merge(&subject, &CostTuple::cents(100), None)
        .await
        .expect("first merge creates the account");
    assert_eq!(after_first.cents, 100, "first top-up funds 100c");

    // The second merge SUMS onto the existing balance.
    let after_second = store
        .provision_merge(&subject, &CostTuple::cents(100), None)
        .await
        .expect("second merge accumulates");
    assert_eq!(
        after_second.cents, 200,
        "additive merge: two 100c top-ups sum to 200c (not the last-write 100c)"
    );

    // And the ledger agrees with what the merge reported.
    let balance = store
        .current_balance(&subject)
        .await
        .expect("the subject is provisioned");
    assert_eq!(balance.cents, 200);
}

#[tokio::test]
async fn merge_accumulates_every_dimension() {
    let subject = HolderId("holder-dims".into());
    let store = InMemoryBudgetStore::new();
    let chunk = CostTuple {
        tokens_in: 10,
        tokens_out: 20,
        cents: 30,
        wall_ms: 40,
        attention_score: 50,
    };

    store.provision_merge(&subject, &chunk, None).await.unwrap();
    store.provision_merge(&subject, &chunk, None).await.unwrap();
    let balance = store
        .provision_merge(&subject, &chunk, None)
        .await
        .expect("third merge");

    assert_eq!(balance.tokens_in, 30);
    assert_eq!(balance.tokens_out, 60);
    assert_eq!(balance.cents, 90);
    assert_eq!(balance.wall_ms, 120);
    assert_eq!(balance.attention_score, 150);
}

#[tokio::test]
async fn gate_provision_for_twice_lets_a_larger_admission_succeed() {
    // Prove the merge through the gate's public surface, with no balance reader:
    // a single 100c top-up cannot admit a 150c envelope, but two can — so the
    // gate's `provision_for` must have summed the budget, not replaced it.
    let subject = HolderId("holder-gate".into());
    let token = TokenId(Uuid::new_v4());

    let gate = InMemoryCostAdmissionGate::new(InMemoryBudgetStore::new());
    gate.bind_token(token, subject.clone());

    gate.provision_for(&subject, CostTuple::cents(100))
        .await
        .expect("first top-up");

    // With only 100c, a 150c envelope cannot be admitted.
    let one_topup = gate.admit(request(token, 150)).await;
    assert!(
        matches!(one_topup, Err(AdmissionError::BudgetExhausted { .. })),
        "100c cannot cover a 150c envelope, got {one_topup:?}"
    );

    // A second 100c top-up sums to 200c; now the 150c envelope admits.
    gate.provision_for(&subject, CostTuple::cents(100))
        .await
        .expect("second top-up");
    gate.admit(request(token, 150))
        .await
        .expect("200c covers the 150c envelope after the additive merge");
}

#[tokio::test]
async fn merge_over_the_per_subject_cap_is_refused_and_balance_is_unchanged() {
    let subject = HolderId("holder-capped".into());
    // Cap the accumulated cents balance at 150.
    let gate = InMemoryCostAdmissionGate::new(InMemoryBudgetStore::new())
        .with_provision_cap(CostTuple::cents(150));

    gate.provision_for(&subject, CostTuple::cents(100))
        .await
        .expect("first 100c is within the 150c cap");

    // A second 100c would reach 200c > 150c cap: refused on the cents dimension.
    let err = gate
        .provision_for(&subject, CostTuple::cents(100))
        .await
        .expect_err("the over-cap merge is refused");
    match err {
        ProvisionError::OverCap {
            subject: s,
            dimension,
        } => {
            assert_eq!(s, subject);
            assert_eq!(dimension, "cents");
        }
        other => panic!("expected OverCap, got {other:?}"),
    }
}

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
