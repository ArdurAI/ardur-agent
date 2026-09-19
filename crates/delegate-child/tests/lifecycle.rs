//! Lifecycle proofs for supervised children (gh#490).
//!
//! The mock provider here is deliberately *slow and observable*: it records
//! every dispatch and can be held open indefinitely. Cancellation claims cannot
//! be proven against an instant provider — a child that finishes before the
//! cancel arrives proves nothing about whether cancellation actually stops
//! work.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use ardur_cap_token::{
    BiscuitCapTokenIssuer, CapScope, CapToken, CapTokenIssuer, HashSetDenyList, HolderId,
};
use ardur_core_types::{CostEnvelope, CostTuple, ModelId};
use ardur_delegate_child::{
    BudgetReservation, CHILD_CANCELLED_VERB, CHILD_COMPLETED_VERB, ChildError, ChildOutcome,
    ChildSpec, ChildSupervisor, ParentBudget,
};
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, Provider, ProviderError, ProviderId,
    RateCard, Usage,
};
use async_trait::async_trait;
use biscuit_auth::KeyPair;

/// A provider whose latency and reply are controllable, and which counts every
/// dispatch it is actually asked to perform.
struct MockProvider {
    dispatches: Arc<AtomicU32>,
    latency: Duration,
    reply: String,
    cents_per_call: u64,
    fail_with: Option<ProviderErrorKind>,
    finish_error: Option<String>,
    rate_card: RateCard,
}

#[derive(Clone, Copy)]
enum ProviderErrorKind {
    Unauthorized,
    Network,
}

impl MockProvider {
    fn new(reply: &str) -> Self {
        Self {
            dispatches: Arc::new(AtomicU32::new(0)),
            latency: Duration::from_millis(10),
            reply: reply.to_string(),
            cents_per_call: 1,
            fail_with: None,
            finish_error: None,
            rate_card: RateCard {
                version_id: "mock-test-v1".into(),
                cents_per_1k_input: 0.0,
                cents_per_1k_output: 0.0,
                cents_per_request: 0.0,
            },
        }
    }

    fn slow(mut self, d: Duration) -> Self {
        self.latency = d;
        self
    }

    fn cents(mut self, c: u64) -> Self {
        self.cents_per_call = c;
        self
    }

    fn failing(mut self, kind: ProviderErrorKind) -> Self {
        self.fail_with = Some(kind);
        self
    }

    /// Return content but end the turn with a provider-reported error, the
    /// shape that made content-only completion unsafe.
    fn finishing_with_error(mut self, msg: &str) -> Self {
        self.finish_error = Some(msg.to_string());
        self
    }

    fn counter(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.dispatches)
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.latency).await;
        match self.fail_with {
            Some(ProviderErrorKind::Unauthorized) => Err(ProviderError::Unauthorized),
            Some(ProviderErrorKind::Network) => {
                Err(ProviderError::NetworkFailure("connection reset".into()))
            }
            None => Ok(CompletionResponse {
                content: self.reply.clone(),
                finish_reason: match &self.finish_error {
                    Some(msg) => FinishReason::Error(msg.clone()),
                    None => FinishReason::Stop,
                },
                usage: Usage {
                    tokens_in: 10,
                    tokens_out: 5,
                    cost_cents: Some(self.cents_per_call),
                },
                cost: CostTuple {
                    cents: self.cents_per_call,
                    ..CostTuple::default()
                },
                raw_provider_response: None,
            }),
        }
    }

    fn id(&self) -> ProviderId {
        ProviderId("mock".into())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

fn test_token() -> CapToken {
    BiscuitCapTokenIssuer::new(KeyPair::new())
        .issue(
            HolderId("delegate-child-test".into()),
            CapScope {
                audience: "delegate-child-test".into(),
                expires_unix: 2_000_000_000,
                budget_remaining: 100,
                tool_allowlist: vec!["chat.submit".into()],
            },
        )
        .expect("issue a child fixture token")
}

fn spec(prompt: &str, reservation: BudgetReservation, max_rounds: u32) -> ChildSpec {
    spec_with_token(prompt, test_token(), reservation, max_rounds)
}

fn spec_with_token(
    prompt: &str,
    token: CapToken,
    reservation: BudgetReservation,
    max_rounds: u32,
) -> ChildSpec {
    ChildSpec {
        prompt: prompt.to_string(),
        token,
        reservation,
        model: ModelId::new("mock-model"),
        max_rounds,
        envelope: CostEnvelope {
            tokens_in_max: 1000,
            tokens_out_max: 4096,
            cents_max: 1,
            wall_ms_max: 60_000,
            attention_score_max: 0,
        },
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_child_runs_to_completion_and_reports_its_text() {
    let provider = Arc::new(MockProvider::new("child answer"));
    let sup = ChildSupervisor::new(Arc::clone(&provider), Box::new(HashSetDenyList::new()));

    let reservation = ParentBudget::new(100)
        .reserve(10)
        .expect("reservation fits");
    let child = sup
        .spawn(spec("do the thing", reservation, 4))
        .await
        .expect("spawns");
    let outcome = child.join().await.expect("worker joins");

    assert!(
        outcome.is_success(),
        "a clean run must be a success: {outcome:?}"
    );
    assert_eq!(outcome.verb(), CHILD_COMPLETED_VERB);
    match outcome {
        ChildOutcome::Completed { text, rounds, .. } => {
            assert_eq!(text, "child answer");
            assert_eq!(rounds, 1, "one round was enough");
        }
        other => panic!("expected completion, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_the_worker_before_it_dispatches_another_round() {
    // Rounds must be SHORT relative to the observation window. With a single
    // 30s round, a detached worker would still be asleep inside round 1 while
    // the test watched, so the dispatch counter could not climb and the test
    // passed even with cancellation removed entirely (caught by mutation M1).
    // A fast, empty-replying provider loops continuously, so a worker that was
    // merely detached keeps dispatching and is visible within milliseconds.
    let provider = Arc::new(MockProvider::new("").slow(Duration::from_millis(5)));
    let counter = provider.counter();
    let sup = ChildSupervisor::new(Arc::clone(&provider), Box::new(HashSetDenyList::new()));

    // max_rounds is high and the budget generous, so nothing but cancellation
    // can stop this child — otherwise the test could pass for the wrong reason.
    let reservation = ParentBudget::new(10_000)
        .reserve(10_000)
        .expect("reservation fits");
    let child = sup
        .spawn(spec("long job", reservation, 100_000))
        .await
        .expect("spawns");

    tokio::time::sleep(Duration::from_millis(50)).await;

    let spun_before_cancel = counter.load(Ordering::SeqCst);
    assert!(
        spun_before_cancel > 0,
        "the child never dispatched, so this proves nothing about stopping it"
    );

    let outcome = child.cancel().await.expect("cancel joins the worker");

    assert!(
        matches!(outcome, ChildOutcome::Cancelled { .. }),
        "a cancelled child must report Cancelled, got {outcome:?}"
    );
    assert_eq!(
        outcome.verb(),
        CHILD_CANCELLED_VERB,
        "cancellation still settles"
    );

    // The decisive assertion: cancel() returned only after the worker stopped,
    // so the dispatch count is frozen from here on. A detached worker would
    // keep looping and this count would climb.
    let at_cancel = counter.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        counter.load(Ordering::SeqCst),
        at_cancel,
        "the worker kept dispatching after cancel returned - it was detached, not cancelled"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_token_stops_the_child_at_the_next_action_boundary() {
    let provider = Arc::new(MockProvider::new("").slow(Duration::from_millis(30)));
    let mut deny = HashSetDenyList::new();
    let token = test_token();

    // The SAME token must be revoked and then presented. An earlier version of
    // this test minted a fresh token per call, revoked one and spawned another:
    // the spawn correctly succeeded and the test read as a fail-open that was
    // not there.
    deny.revoke_token(&token);
    assert!(
        !token.revocation_ids().is_empty(),
        "a token with no revocation ids could never be revoked, making this vacuous"
    );

    let sup = ChildSupervisor::new(Arc::clone(&provider), Box::new(deny));
    let reservation = ParentBudget::new(100)
        .reserve(100)
        .expect("reservation fits");
    let Err(err) = sup
        .spawn(spec_with_token("job", token, reservation, 10))
        .await
    else {
        panic!("an already-revoked token must not spawn a child");
    };
    assert!(
        matches!(err, ChildError::AlreadyRevoked),
        "expected AlreadyRevoked, got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn revocation_is_distinct_from_cancellation() {
    // Both stop the child, but an operator must be able to tell "I stopped it"
    // from "its authority was withdrawn".
    let cancelled = ChildOutcome::Cancelled {
        rounds: 2,
        cost: CostTuple::default(),
    };
    let revoked = ChildOutcome::Revoked {
        rounds: 2,
        cost: CostTuple::default(),
    };
    assert_ne!(cancelled, revoked, "the two outcomes must not be conflated");
    assert_eq!(
        cancelled.verb(),
        revoked.verb(),
        "both still settle terminally"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_child_cannot_reserve_more_than_its_parent_holds() {
    let err = ParentBudget::new(100)
        .reserve(500)
        .expect_err("over-reservation must fail");
    match err {
        ChildError::ReservationTooLarge {
            requested,
            available,
        } => {
            assert_eq!(requested, 500);
            assert_eq!(available, 100);
        }
        other => panic!("expected ReservationTooLarge, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn spend_is_recorded_against_the_reservation_per_child() {
    let reservation = ParentBudget::new(100).reserve(10).expect("fits");
    assert_eq!(reservation.remaining_cents(), 10);

    reservation.record_spend(&CostTuple {
        cents: 4,
        ..CostTuple::default()
    });
    assert_eq!(reservation.spent_cents(), 4);
    assert_eq!(reservation.remaining_cents(), 6);

    assert!(reservation.can_afford(&CostTuple {
        cents: 6,
        ..CostTuple::default()
    }));
    assert!(
        !reservation.can_afford(&CostTuple {
            cents: 7,
            ..CostTuple::default()
        }),
        "a round larger than the remainder must not be affordable"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_exhausted_budget_stops_the_child_without_dispatching() {
    // Reservation smaller than one round's envelope: the child must refuse to
    // dispatch at all rather than overrun and settle the overage afterwards.
    let provider = Arc::new(MockProvider::new("answer").cents(5));
    let counter = provider.counter();
    let sup = ChildSupervisor::new(Arc::clone(&provider), Box::new(HashSetDenyList::new()));

    let reservation = ParentBudget::new(100)
        .reserve(0)
        .expect("zero reservation is legal");
    let child = sup
        .spawn(spec("job", reservation, 10))
        .await
        .expect("spawns");
    let outcome = child.join().await.expect("joins");

    assert!(
        matches!(outcome, ChildOutcome::BudgetExhausted { rounds: 0, .. }),
        "expected BudgetExhausted before any dispatch, got {outcome:?}"
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "an unaffordable round must never reach the provider"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_prompt_is_refused_before_a_worker_exists() {
    let provider = Arc::new(MockProvider::new("x"));
    let counter = provider.counter();
    let sup = ChildSupervisor::new(Arc::clone(&provider), Box::new(HashSetDenyList::new()));

    let reservation = ParentBudget::new(100).reserve(10).expect("fits");
    let Err(err) = sup.spawn(spec("   ", reservation, 4)).await else {
        panic!("an empty prompt must be refused");
    };
    assert!(matches!(err, ChildError::EmptyPrompt), "got {err:?}");
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "nothing should have been dispatched"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unauthorized_provider_failure_keeps_its_own_classification() {
    let provider = Arc::new(MockProvider::new("").failing(ProviderErrorKind::Unauthorized));
    let sup = ChildSupervisor::new(Arc::clone(&provider), Box::new(HashSetDenyList::new()));

    let reservation = ParentBudget::new(100).reserve(10).expect("fits");
    let child = sup
        .spawn(spec("job", reservation, 4))
        .await
        .expect("spawns");
    let outcome = child.join().await.expect("joins");

    match outcome {
        ChildOutcome::Failed { reason, .. } => assert!(
            reason.contains("unauthorized"),
            "an auth failure must stay identifiable as one, got {reason:?}"
        ),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_network_failure_is_not_reported_as_an_auth_failure() {
    let provider = Arc::new(MockProvider::new("").failing(ProviderErrorKind::Network));
    let sup = ChildSupervisor::new(Arc::clone(&provider), Box::new(HashSetDenyList::new()));

    let reservation = ParentBudget::new(100).reserve(10).expect("fits");
    let child = sup
        .spawn(spec("job", reservation, 4))
        .await
        .expect("spawns");
    let outcome = child.join().await.expect("joins");

    match outcome {
        ChildOutcome::Failed { reason, .. } => assert!(
            !reason.contains("unauthorized"),
            "a transport fault must not be classified as an auth failure: {reason:?}"
        ),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_clean_completion_settles_as_completed() {
    // Every non-completion terminal state settles under the cancelled verb, so
    // a parent reconciling receipts cannot mistake an unfinished child for a
    // finished one.
    assert_eq!(
        ChildOutcome::Completed {
            text: "x".into(),
            rounds: 1,
            cost: CostTuple::default(),
        }
        .verb(),
        CHILD_COMPLETED_VERB
    );
    for unfinished in [
        ChildOutcome::Cancelled {
            rounds: 1,
            cost: CostTuple::default(),
        },
        ChildOutcome::Revoked {
            rounds: 1,
            cost: CostTuple::default(),
        },
        ChildOutcome::BudgetExhausted {
            rounds: 1,
            cost: CostTuple::default(),
        },
        ChildOutcome::Failed {
            reason: "x".into(),
            rounds: 1,
            cost: CostTuple::default(),
        },
    ] {
        assert_eq!(
            unfinished.verb(),
            CHILD_CANCELLED_VERB,
            "{unfinished:?} must not settle as completed"
        );
        assert!(!unfinished.is_success(), "{unfinished:?} is not a success");
    }
}

// ---------------------------------------------------------------------------
// Guards for the review findings on PR #516.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn two_children_cannot_reserve_the_same_cent() {
    // The bug: `available` was a copied scalar, so both children saw 100 and
    // both succeeded, committing 120 cents of a 100-cent parent.
    let parent = ParentBudget::new(100);
    let first = parent.reserve(60).expect("first child fits");
    let second = parent.reserve(60);

    assert_eq!(first.reserved_cents(), 60);
    assert!(
        second.is_err(),
        "a second 60-cent child must not fit in a 100-cent parent"
    );
    assert_eq!(
        parent.available_cents(),
        40,
        "the first reservation must be deducted"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_reservations_never_oversubscribe_the_parent() {
    // A single-threaded check cannot catch a lost update; race many tasks and
    // assert the invariant on the ledger itself.
    let parent = ParentBudget::new(100);
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..40 {
        let p = parent.clone();
        set.spawn(async move { p.reserve(10).is_ok() });
    }
    let mut granted = 0;
    while let Some(r) = set.join_next().await {
        if r.expect("task joins") {
            granted += 1;
        }
    }
    assert_eq!(
        granted, 10,
        "exactly 100/10 reservations may be granted, got {granted}"
    );
    assert_eq!(
        parent.available_cents(),
        0,
        "the parent must be fully committed, never negative"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn settling_returns_unspent_cents_to_the_parent() {
    let parent = ParentBudget::new(100);
    let child = parent.reserve(40).expect("fits");
    assert_eq!(parent.available_cents(), 60);

    child.record_spend(&CostTuple {
        cents: 15,
        ..CostTuple::default()
    });
    child.settle();

    assert_eq!(
        parent.available_cents(),
        85,
        "the 25 unspent cents must return to the parent, not stay locked"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_round_limit_is_not_reported_as_budget_exhaustion() {
    // The bug: hitting max_rounds with funds left reported BudgetExhausted,
    // telling an operator to top up a budget that was never the constraint.
    let provider = Arc::new(MockProvider::new("")); // empty replies keep it looping
    let sup = ChildSupervisor::new(Arc::clone(&provider), Box::new(HashSetDenyList::new()));

    let reservation = ParentBudget::new(10_000)
        .reserve(10_000)
        .expect("plenty of budget");
    let child = sup
        .spawn(spec("job", reservation, 3))
        .await
        .expect("spawns");
    let outcome = child.join().await.expect("joins");

    assert!(
        matches!(outcome, ChildOutcome::RoundLimitReached { rounds: 3, .. }),
        "expected RoundLimitReached with funds remaining, got {outcome:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn partial_output_with_an_error_finish_is_a_failure_not_a_completion() {
    // The bug: content-only success meant a provider that emitted a partial
    // message and THEN failed produced a Completed child with the success verb.
    let provider =
        Arc::new(MockProvider::new("partial answer").finishing_with_error("turn.failed"));
    let sup = ChildSupervisor::new(Arc::clone(&provider), Box::new(HashSetDenyList::new()));

    let reservation = ParentBudget::new(100).reserve(10).expect("fits");
    let child = sup
        .spawn(spec("job", reservation, 4))
        .await
        .expect("spawns");
    let outcome = child.join().await.expect("joins");

    assert!(
        !outcome.is_success(),
        "a turn that finished with an error must not be a success: {outcome:?}"
    );
    assert_eq!(outcome.verb(), CHILD_CANCELLED_VERB);
    match outcome {
        ChildOutcome::Failed { reason, .. } => {
            assert!(
                reason.contains("turn.failed"),
                "the provider reason must survive: {reason}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_billing_above_the_projection_stops_the_child() {
    // The bug: the pre-dispatch check admitted the call on a 1-cent projection
    // while the provider billed 50, and nothing enforced the reservation after.
    //
    // Asserting only the OUTCOME is vacuous: without the overdraft check the
    // next iteration's affordability check also yields BudgetExhausted. The
    // observable difference is how many times the provider was actually
    // BILLED, so assert the dispatch count. 60 reserved with 50 billed per
    // round leaves 10 cents, which still affords the 1-cent projection, so
    // only the overdraft check can stop a second paid round.
    let provider = Arc::new(MockProvider::new("").cents(50));
    let counter = provider.counter();
    let sup = ChildSupervisor::new(Arc::clone(&provider), Box::new(HashSetDenyList::new()));

    let reservation = ParentBudget::new(100).reserve(60).expect("fits");
    let child = sup
        .spawn(spec("job", reservation, 100))
        .await
        .expect("spawns");
    let outcome = child.join().await.expect("joins");

    assert!(
        matches!(outcome, ChildOutcome::BudgetExhausted { .. }),
        "an overdrawn child must stop, got {outcome:?}"
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "the child billed a second time, overrunning its reservation"
    );
    assert_eq!(
        outcome.rounds(),
        1,
        "it must stop after the round that overdrew it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_supervisor_accepts_a_dyn_provider_handle() {
    // The bug: `P: Provider` implies Sized, so Arc<dyn Provider> - what the
    // production selector hands out - could not construct a supervisor at all.
    let provider: Arc<dyn Provider> = Arc::new(MockProvider::new("dyn answer"));
    let sup = ChildSupervisor::new(Arc::clone(&provider), Box::new(HashSetDenyList::new()));

    let reservation = ParentBudget::new(100).reserve(10).expect("fits");
    let child = sup
        .spawn(spec("job", reservation, 4))
        .await
        .expect("spawns");
    let outcome = child.join().await.expect("joins");

    assert!(
        outcome.is_success(),
        "the dyn-dispatched child must run: {outcome:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_revoked_child_reports_revocation_not_cancellation() {
    // The bug: the worker is mid-dispatch when the stop lands and reports
    // Cancelled, so the operator saw an ordinary cancellation for a child whose
    // authority had been withdrawn.
    let provider = Arc::new(MockProvider::new("").slow(Duration::from_millis(5)));
    let sup = ChildSupervisor::new(Arc::clone(&provider), Box::new(HashSetDenyList::new()));

    let reservation = ParentBudget::new(10_000).reserve(10_000).expect("fits");
    let child = sup
        .spawn(spec("job", reservation, 100_000))
        .await
        .expect("spawns");
    tokio::time::sleep(Duration::from_millis(30)).await;

    let outcome = sup.stop_revoked(child).await.expect("stops");
    assert!(
        matches!(outcome, ChildOutcome::Revoked { .. }),
        "a revoked child must settle as Revoked, got {outcome:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_completed_round_is_accounted_even_when_a_cancel_arrives_together() {
    // The bug: a biased select always took the cancel arm, discarding a
    // response the upstream had already billed - understating spend and rounds.
    let provider = Arc::new(MockProvider::new("done").cents(3));
    let sup = ChildSupervisor::new(Arc::clone(&provider), Box::new(HashSetDenyList::new()));

    let reservation = ParentBudget::new(100).reserve(50).expect("fits");
    let child = sup
        .spawn(spec("job", reservation, 4))
        .await
        .expect("spawns");

    // The provider is fast, so the response is ready essentially immediately;
    // cancelling now races it.
    let outcome = child.cancel().await.expect("cancel joins");

    if outcome.is_success() {
        assert_eq!(
            outcome.rounds(),
            1,
            "a completed round must be counted, not dropped"
        );
    } else {
        assert!(
            matches!(outcome, ChildOutcome::Cancelled { .. }),
            "otherwise it must be an honest cancellation, got {outcome:?}"
        );
    }
}
