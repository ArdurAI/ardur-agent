//! Benchmarks for `ardur-cost-gate` — the admission cycle every metered call
//! routes through.
//!
//! The gate runs on the request hot path: `admit` (stages 1–3: resolve holder,
//! screen ceilings, atomically reserve the envelope against the budget) hands
//! back a `Reservation`, and `finalize` (stage 4: post actual cost, refund the
//! unspent delta) closes it. We measure the full `admit → finalize → commit`
//! cycle — the realistic unit of work — plus the stage-2 deny fast path, which
//! rejects before any reservation and is the common case under a tight ceiling.
//!
//! The cost is lock acquisition (`parking_lot::RwLock` over the reservation /
//! finalized / token-holder tables), two `HashMap` mutations per cycle, a
//! UUIDv4 mint for the reservation id, and the `async_trait` future machinery.
//! `commit_finalization` is called each cycle to drop the finalized-rollback
//! entry, so the tables stay bounded and the benchmark reaches a steady state
//! rather than growing a map for the whole run.
//!
//! `actual = ZERO` makes finalize refund the entire reserved envelope, so a
//! holder's balance is conserved across iterations — the benchmark never
//! exhausts the (large) provisioned budget.

use std::hint::black_box;

use ardur_cost_gate::{
    AdmissionError, AdmissionRequest, CostAdmissionGate, CostEnvelope, CostTuple, HolderId,
    InMemoryBudgetStore, InMemoryCostAdmissionGate, ModelId, ProviderId, Sha256Digest, TokenId,
};
use criterion::{Criterion, criterion_group, criterion_main};
use uuid::Uuid;

/// A gate with a single holder provisioned to a large budget and one bound
/// token — the steady-state setup a server holds per turn.
fn setup() -> (
    InMemoryCostAdmissionGate<InMemoryBudgetStore>,
    tokio::runtime::Runtime,
    TokenId,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let budget = InMemoryBudgetStore::new();
    let gate = InMemoryCostAdmissionGate::new(budget);
    let holder = HolderId("bench-holder".to_string());
    let token = TokenId(Uuid::new_v4());
    gate.bind_token(token, holder.clone());
    rt.block_on(gate.provision_for(
        &holder,
        // Huge budget: conserved across iterations (finalize refunds in full),
        // so it never runs dry.
        CostTuple {
            tokens_in: u64::from(u32::MAX),
            tokens_out: u64::from(u32::MAX),
            cents: u64::from(u32::MAX),
            wall_ms: u64::from(u32::MAX),
            attention_score: u64::from(u32::MAX),
        },
    ))
    .expect("provision");
    (gate, rt, token)
}

fn request(token: TokenId) -> AdmissionRequest {
    AdmissionRequest {
        cap_token_id: token,
        projected_envelope: CostEnvelope {
            tokens_in_max: 1_000,
            tokens_out_max: 1_000,
            cents_max: 10,
            wall_ms_max: 5_000,
            attention_score_max: 1,
        },
        provider_id: ProviderId("openrouter".to_string()),
        model_id: ModelId("gpt-4".to_string()),
        request_digest: Sha256Digest::of(b"bench-request"),
    }
}

fn bench_admit_finalize(c: &mut Criterion) {
    let (gate, rt, token) = setup();
    c.bench_function("cost_gate/admit_finalize_commit", |b| {
        b.iter(|| {
            rt.block_on(async {
                let reservation = gate.admit(black_box(request(token))).await.unwrap();
                let id = reservation.reservation_id;
                // actual = ZERO → full refund → budget conserved for the next iter.
                gate.finalize(reservation, CostTuple::ZERO).await.unwrap();
                // Drop the rollback-state entry so the finalized table stays
                // bounded (mirrors the durable-commit step in production).
                gate.commit_finalization(id).await;
            });
        });
    });
}

/// The rejection fast path: a request whose envelope exceeds a hard per-call
/// ceiling is denied in stage 2, before any budget reservation. No reservation
/// is created and no finalize/cleanup is needed, so this is a self-contained
/// measurement of the deny path — the common case under an aggressive ceiling.
fn bench_admit_denied(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let gate =
        InMemoryCostAdmissionGate::new(InMemoryBudgetStore::new()).with_ceiling(CostEnvelope {
            tokens_in_max: 10,
            tokens_out_max: 10,
            cents_max: 1,
            wall_ms_max: 10,
            attention_score_max: 1,
        });
    let holder = HolderId("bench-holder".to_string());
    let token = TokenId(Uuid::new_v4());
    gate.bind_token(token, holder);
    c.bench_function("cost_gate/admit_denied_ceiling", |b| {
        b.iter(|| {
            rt.block_on(async {
                // The request envelope (1000 tokens, 10c) blows past the 1c
                // ceiling → PolicyDenied without touching the budget store.
                let err = gate.admit(black_box(request(token))).await.unwrap_err();
                // `assert!` (not `debug_assert!`): the bench profile is optimized
                // with debug-assertions off, so a `debug_assert!` would compile to
                // nothing — leaving `err`/`AdmissionError` unused under
                // `-D warnings`. The check is negligible next to the admit work and
                // guards that this really exercises the deny path.
                assert!(matches!(err, AdmissionError::PolicyDenied(_)));
                err
            });
        });
    });
}

criterion_group!(benches, bench_admit_finalize, bench_admit_denied);
criterion_main!(benches);
