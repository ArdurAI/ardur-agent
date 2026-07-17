//! §11.14 Phase 1 — the optimistic store under contention: 10 tasks each try to
//! reserve 200c against a 1000c budget. Exactly five claim it (5 × 200 = 1000);
//! the other five lose the race. The outcome is capacity-determined, not
//! scheduling-determined, because `try_reserve` retries on version conflict and
//! only surfaces `RaceLost` once the balance can no longer cover the request.

use std::sync::Arc;

use ardur_cost_gate::{
    BudgetError, BudgetStore, CostEnvelope, CostTuple, HolderId, InMemoryBudgetStore,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_reservers_against_a_five_slot_budget() {
    let holder = HolderId("holder-1".into());
    let store = Arc::new(InMemoryBudgetStore::new());
    store.set_balance(holder.clone(), CostTuple::cents(1000));

    let envelope = CostEnvelope {
        cents_max: 200,
        ..Default::default()
    };

    let mut tasks = Vec::with_capacity(10);
    for _ in 0..10 {
        let store = Arc::clone(&store);
        let holder = holder.clone();
        tasks.push(tokio::spawn(async move {
            store.try_reserve(&holder, &envelope).await
        }));
    }

    let mut winners = 0;
    let mut losers = 0;
    for task in tasks {
        match task.await.expect("task joins") {
            Ok(_) => winners += 1,
            Err(BudgetError::RaceLost) => losers += 1,
            Err(other) => panic!("unexpected budget error: {other:?}"),
        }
    }

    assert_eq!(winners, 5, "exactly five reservations fit the 1000c budget");
    assert_eq!(losers, 5, "the remaining five lose the race");

    // The budget is fully claimed.
    let remaining = store.current_balance(&holder).await.expect("balance").cents;
    assert_eq!(remaining, 0);
}
