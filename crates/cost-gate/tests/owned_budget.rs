//! Process-local owned-budget lifecycle regressions.
use ardur_cost_gate::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Clone, Default)]
struct FaultStore {
    inner: Arc<InMemoryBudgetStore>,
    fail_next: Arc<AtomicBool>,
    sync_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl BudgetStore for FaultStore {
    async fn current_balance(&self, holder: &HolderId) -> Result<CostTuple, BudgetError> {
        self.inner.current_balance(holder).await
    }
    async fn try_reserve(
        &self,
        holder: &HolderId,
        envelope: &CostEnvelope,
    ) -> Result<ReservationHandle, BudgetError> {
        self.inner.try_reserve(holder, envelope).await
    }
    async fn refund(
        &self,
        handle: ReservationHandle,
        delta: CostDelta,
    ) -> Result<(CostTuple, CostTuple), BudgetError> {
        self.inner.refund(handle, delta).await
    }
    async fn provision_merge(
        &self,
        holder: &HolderId,
        add: &CostTuple,
        cap: Option<&CostTuple>,
    ) -> Result<CostTuple, BudgetError> {
        self.inner.provision_merge(holder, add, cap).await
    }
}

impl SyncBudgetStore for FaultStore {
    fn refund_sync_with_balances(
        &self,
        handle: ReservationHandle,
        delta: CostDelta,
    ) -> Result<(CostTuple, CostTuple), BudgetError> {
        self.sync_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(BudgetError::Internal(anyhow::anyhow!(
                "controlled pre-mutation failure"
            )));
        }
        self.inner.refund_sync_with_balances(handle, delta)
    }
}

async fn fault_fixture(
    balance: u64,
) -> (
    InMemoryCostAdmissionGate<FaultStore>,
    FaultStore,
    HolderId,
    TokenId,
) {
    let store = FaultStore::default();
    let holder = HolderId("fault-holder".into());
    store.inner.set_balance(holder.clone(), all(balance));
    let gate = InMemoryCostAdmissionGate::new(store.clone());
    let token = TokenId(Uuid::new_v4());
    gate.bind_token(token, holder.clone());
    (gate, store, holder, token)
}
use uuid::Uuid;

type Gate = InMemoryCostAdmissionGate<InMemoryBudgetStore>;

async fn fixture(balance: u64) -> (Gate, Arc<ManualClock>, HolderId, TokenId) {
    let clock = Arc::new(ManualClock::new(UnixTsMillis(0)));
    let store = InMemoryBudgetStore::new();
    let holder = HolderId("owner".into());
    store.set_balance(holder.clone(), all(balance));
    let gate = InMemoryCostAdmissionGate::with_clock(store, clock.clone());
    let token = TokenId(Uuid::new_v4());
    gate.bind_token(token, holder.clone());
    (gate, clock, holder, token)
}

fn request(token: TokenId, amount: u32) -> AdmissionRequest {
    AdmissionRequest {
        cap_token_id: token,
        projected_envelope: envelope(amount),
        provider_id: ProviderId("fixture".into()),
        model_id: ModelId("fixture".into()),
        request_digest: Sha256Digest::of(b"fixture"),
    }
}

#[tokio::test]
async fn claim_once_across_independent_calls() {
    let (gate, _, _, token) = fixture(100).await;
    let r = gate.admit(request(token, 10)).await.unwrap();
    let owner = gate.claim_owned(&r).unwrap();
    assert_eq!(owner.reservation_id(), r.reservation_id);
    assert!(
        gate.claim_owned(&r).is_err(),
        "a second call must not mint another owner"
    );
}

#[tokio::test]
async fn owned_admission_release_once_without_generic_theft() {
    let (gate, _, holder, token) = fixture(100).await;
    let r = gate.admit(request(token, 10)).await.unwrap();
    let mut owner = gate.claim_owned(&r).unwrap();
    assert!(gate.take_reservation(r.reservation_id).is_none());
    assert!(gate.finalize(r.clone(), all(5)).await.is_err());
    let view = gate.release_owned_sync(&mut owner, all(7)).unwrap();
    assert_eq!(view.status, OwnedBudgetStatus::Released);
    assert_eq!(view.holder, holder);
    assert_eq!(view.attempt.unwrap().known_incurred, all(7));
    assert_eq!(view.attempt.unwrap().requested_debit, all(0));
    assert_eq!(view.refund.as_ref().unwrap().applied_credit, all(10));
    assert_eq!(gate.release_owned_sync(&mut owner, all(7)).unwrap(), view);
    assert!(gate.pending_owned(r.reservation_id).is_none());
    // Exact capacity proof without access to mutable store internals.
    assert!(gate.admit(request(token, 101)).await.is_err());
    assert!(gate.admit(request(token, 100)).await.is_ok());
}

#[tokio::test]
async fn lost_raw_token_retains_pending_state_without_implicit_refund() {
    let (gate, clock, _, token) = fixture(10).await;
    let r = gate.admit(request(token, 10)).await.unwrap();
    let owner = gate.claim_owned(&r).unwrap();
    drop(owner);
    clock.advance(1_000_000);
    assert_eq!(
        gate.pending_owned(r.reservation_id).unwrap().status,
        OwnedBudgetStatus::Active
    );
    assert!(gate.claim_owned(&r).is_err());
    assert!(gate.take_reservation(r.reservation_id).is_none());
    assert!(gate.finalize(r, all(0)).await.is_err());
    assert!(gate.admit(request(token, 1)).await.is_err());
}

#[tokio::test]
async fn foreign_gate_cannot_use_even_a_closed_owner() {
    let (gate, _, _, token) = fixture(100).await;
    let (other, _, _, _) = fixture(100).await;
    let r = gate.admit(request(token, 10)).await.unwrap();
    let mut owner = gate.claim_owned(&r).unwrap();
    assert!(other.release_owned_sync(&mut owner, all(0)).is_err());
    gate.release_owned_sync(&mut owner, all(0)).unwrap();
    assert!(
        other.release_owned_sync(&mut owner, all(0)).is_err(),
        "even closed evidence is gate-bound"
    );
}

#[derive(Default)]
struct PanicClock {
    armed: AtomicBool,
}

impl Clock for PanicClock {
    fn now_ms(&self) -> UnixTsMillis {
        assert!(
            !self.armed.swap(false, Ordering::SeqCst),
            "controlled one-shot clock panic"
        );
        UnixTsMillis(123)
    }
}

async fn assert_clock_panic_before_application(requested: u64) {
    // Healthy control first; all balances come from the real in-memory ledger.
    for panic in [false, true] {
        let clock = Arc::new(PanicClock::default());
        let store = FaultStore::default();
        let holder = HolderId("clock-holder".into());
        store.inner.set_balance(holder.clone(), all(100));
        let gate = InMemoryCostAdmissionGate::with_clock(store.clone(), clock.clone());
        let token = TokenId(Uuid::new_v4());
        gate.bind_token(token, holder.clone());
        assert_eq!(store.current_balance(&holder).await.unwrap(), all(100));
        let r = gate.admit(request(token, 10)).await.unwrap();
        let mut owner = gate.claim_owned(&r).unwrap();
        let before = gate.owned_view(&owner).unwrap();
        assert_eq!(store.current_balance(&holder).await.unwrap(), all(90));
        assert_eq!(store.sync_calls.load(Ordering::SeqCst), 0);

        let mut after_panic = None;
        if panic {
            clock.armed.store(true, Ordering::SeqCst);
            let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                gate.finalize_owned_sync(&mut owner, all(requested), all(requested))
            }));
            let payload = failure.expect_err("armed clock must actually panic");
            assert_eq!(
                payload.downcast_ref::<&str>(),
                Some(&"controlled one-shot clock panic")
            );
            let pending = gate.pending_owned(r.reservation_id).unwrap();
            assert_eq!(pending.status, OwnedBudgetStatus::Active);
            assert!(pending.application.is_none());
            after_panic = Some((
                store.current_balance(&holder).await.unwrap(),
                store.sync_calls.load(Ordering::SeqCst),
                pending,
            ));
        }
        let view = gate
            .finalize_owned_sync(&mut owner, all(requested), all(requested))
            .unwrap();
        let after_retry = store.current_balance(&holder).await.unwrap();
        let retry_calls = store.sync_calls.load(Ordering::SeqCst);
        let rolled_back = gate.rollback_owned_sync(&mut owner).unwrap();
        let after_rollback = store.current_balance(&holder).await.unwrap();
        let rollback_calls = store.sync_calls.load(Ordering::SeqCst);
        // Capture the entire observed trajectory before the boundary assertion:
        // a RED run must show real duplicate application, not predicted balances.
        eprintln!(
            "clock panic={panic} requested={requested}: post-panic={after_panic:?}; \
             retry={after_retry:?}/{retry_calls} calls; \
             rollback={after_rollback:?}/{rollback_calls} calls"
        );
        if let Some((balance, calls, pending)) = after_panic {
            assert_eq!(balance, all(90), "clock panic must precede ledger mutation");
            assert_eq!(calls, 0, "clock panic must precede the sync store call");
            assert_eq!(
                pending, before,
                "clock panic must leave ownership unchanged"
            );
        }
        assert_eq!(view.status, OwnedBudgetStatus::Finalized);
        let app = view.application.unwrap();
        assert_eq!(app.balance_before, all(90));
        assert_eq!(app.balance_after, all(100 - requested));
        assert_eq!(app.applied_debit, all(requested));
        assert_eq!(app.receipt.actual, all(requested));
        assert_eq!(app.receipt.finalized_at, UnixTsMillis(123));
        assert_eq!(after_retry, all(100 - requested));
        assert_eq!(retry_calls, 1);
        assert_eq!(rolled_back.status, OwnedBudgetStatus::RolledBack);
        assert_eq!(
            rolled_back.refund.as_ref().unwrap().applied_credit,
            all(requested)
        );
        assert_eq!(after_rollback, all(100));
        assert_eq!(rollback_calls, 2);
        assert_eq!(gate.rollback_owned_sync(&mut owner).unwrap(), rolled_back);
        assert_eq!(store.sync_calls.load(Ordering::SeqCst), 2);
        assert!(gate.pending_owned(r.reservation_id).is_none());
    }
}

#[tokio::test]
async fn owned_clock_panic_precedes_hold_credit() {
    assert_clock_panic_before_application(6).await;
}

#[tokio::test]
async fn owned_clock_panic_precedes_overrun_debit() {
    assert_clock_panic_before_application(20).await;
}

#[derive(Default)]
struct CallbackClock {
    action: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Clock for CallbackClock {
    fn now_ms(&self) -> UnixTsMillis {
        let action = self.action.lock().unwrap().take();
        if let Some(action) = action {
            action();
        }
        UnixTsMillis(123)
    }
}

// A synchronous lock regression cannot be bounded by Tokio's timeout. Reexec
// only the selected test and kill/reap its child on timeout, never a live thread.
fn bounded_clock_child(test: &str, mode: &str) {
    use std::io::Read;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    struct ReapOnDrop(Child);
    impl Drop for ReapOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = ReapOnDrop(
        Command::new(std::env::current_exe().unwrap())
            .args([test, "--exact", "--nocapture", "--test-threads=1"])
            .env("ARDUR_OWNED_CLOCK_CHILD", mode)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stderr = child.0.stderr.take().unwrap();
    // Drain continuously, retaining a bounded diagnostic buffer.
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let read = stderr.read(&mut buffer).unwrap();
            if read == 0 {
                return String::from_utf8_lossy(&output).into_owned();
            }
            let keep = read.min(65_536 - output.len());
            output.extend_from_slice(&buffer[..keep]);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let (status, timed_out) = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break (status, false);
        }
        if Instant::now() >= deadline {
            child.0.kill().unwrap();
            break (child.0.wait().unwrap(), true);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = reader.join().unwrap();
    eprintln!("child {test}/{mode}: {status}; timed_out={timed_out}; reaped\n{output}");
    assert!(
        output.contains("CLOCK_CALLBACK_ENTERED"),
        "child must reach clock callback"
    );
    assert!(
        !timed_out,
        "injected clock reentry must complete without a held gate lock: {test}; child killed and reaped"
    );
    assert!(
        status.success(),
        "clock child failed: {test}/{mode}\n{output}"
    );
    assert!(output.contains("CLOCK_CALLBACK_RETURNED"));
    assert!(output.contains("CLOCK_CLEANUP_EXACT"));
}

async fn clock_reentry_case(claim: bool, reenter: bool) {
    let store = FaultStore::default();
    let clock = Arc::new(CallbackClock::default());
    let holder = HolderId("callback-holder".into());
    store.inner.set_balance(holder.clone(), all(100));
    let gate = Arc::new(InMemoryCostAdmissionGate::with_clock(
        store.clone(),
        clock.clone(),
    ));
    let token = TokenId(Uuid::new_v4());
    gate.bind_token(token, holder.clone());
    let r = gate.admit(request(token, 10)).await.unwrap();
    assert_ne!(r.reservation_id, Uuid::nil());
    let mut owner = if claim {
        None
    } else {
        Some(gate.claim_owned(&r).unwrap())
    };
    assert_eq!(store.current_balance(&holder).await.unwrap(), all(90));
    assert_eq!(store.sync_calls.load(Ordering::SeqCst), 0);
    let weak = Arc::downgrade(&gate);
    let id = r.reservation_id;
    *clock.action.lock().unwrap() = Some(Box::new(move || {
        eprintln!("CLOCK_CALLBACK_ENTERED");
        if reenter {
            let gate = weak.upgrade().unwrap();
            if claim {
                // The admitted v4 id is never nil: querying this id cannot steal
                // its hold, but must acquire the ordinary reservations lock.
                assert!(gate.take_reservation(Uuid::nil()).is_none());
            } else {
                let view = gate.pending_owned(id).unwrap();
                assert_eq!(view.status, OwnedBudgetStatus::Active);
                assert!(view.application.is_none());
            }
        }
        eprintln!("CLOCK_CALLBACK_RETURNED");
    }));
    if claim {
        owner = Some(gate.claim_owned(&r).unwrap());
    } else {
        let view = gate
            .finalize_owned_sync(owner.as_mut().unwrap(), all(6), all(6))
            .unwrap();
        let app = view.application.unwrap();
        assert_eq!(app.balance_before, all(90));
        assert_eq!(app.balance_after, all(94));
        assert_eq!(app.applied_debit, all(6));
        assert_eq!(app.receipt.finalized_at, UnixTsMillis(123));
        assert_eq!(store.sync_calls.load(Ordering::SeqCst), 1);
    }
    let mut owner = owner.unwrap();
    assert!(
        clock.action.lock().unwrap().is_none(),
        "callback must actually execute"
    );
    assert!(gate.take_reservation(r.reservation_id).is_none());
    let closed = if claim {
        let view = gate.owned_view(&owner).unwrap();
        assert_eq!(view.status, OwnedBudgetStatus::Active);
        assert!(view.attempt.is_none());
        assert!(view.application.is_none());
        assert_eq!(store.current_balance(&holder).await.unwrap(), all(90));
        assert_eq!(store.sync_calls.load(Ordering::SeqCst), 0);
        gate.release_owned_sync(&mut owner, all(0)).unwrap()
    } else {
        assert_eq!(store.current_balance(&holder).await.unwrap(), all(94));
        gate.rollback_owned_sync(&mut owner).unwrap()
    };
    let refund = closed.refund.as_ref().unwrap();
    assert_eq!(refund.applied_credit, all(if claim { 10 } else { 6 }));
    assert_eq!(refund.remaining_credit, all(0));
    assert_eq!(store.current_balance(&holder).await.unwrap(), all(100));
    assert_eq!(
        store.sync_calls.load(Ordering::SeqCst),
        if claim { 1 } else { 2 }
    );
    assert_eq!(
        closed.status,
        if claim {
            OwnedBudgetStatus::Released
        } else {
            OwnedBudgetStatus::RolledBack
        }
    );
    assert_eq!(gate.owned_view(&owner).unwrap(), closed);
    assert!(gate.pending_owned(r.reservation_id).is_none());
    eprintln!("CLOCK_CLEANUP_EXACT");
}

async fn assert_clock_reentry(test: &str, claim: bool) {
    if let Ok(mode) = std::env::var("ARDUR_OWNED_CLOCK_CHILD") {
        assert!(mode == "control" || mode == "reenter");
        clock_reentry_case(claim, mode == "reenter").await;
    } else {
        bounded_clock_child(test, "control");
        bounded_clock_child(test, "reenter");
    }
}

#[tokio::test]
async fn owned_clock_reentry_during_finalize() {
    assert_clock_reentry("owned_clock_reentry_during_finalize", false).await;
}

#[tokio::test]
async fn owned_clock_reentry_during_claim() {
    assert_clock_reentry("owned_clock_reentry_during_claim", true).await;
}

#[tokio::test]
async fn owned_finalization_applies_hold_credit_once() {
    let (gate, _, _, token) = fixture(100).await;
    let r = gate.admit(request(token, 10)).await.unwrap();
    let mut owner = gate.claim_owned(&r).unwrap();
    let view = gate
        .finalize_owned_sync(&mut owner, all(6), all(6))
        .unwrap();
    assert_eq!(view.status, OwnedBudgetStatus::Finalized);
    let applied = view.application.as_ref().unwrap();
    assert_eq!(applied.reserved_credit, all(4));
    assert_eq!(applied.additional_debit, all(0));
    assert_eq!(applied.applied_debit, all(6));
    assert_eq!(applied.shortfall, all(0));
    assert_eq!(applied.balance_before, all(90));
    assert_eq!(applied.balance_after, all(94));
    assert_eq!(applied.receipt.actual, all(6));
    assert!(
        gate.finalize_owned_sync(&mut owner, all(6), all(6))
            .is_err()
    );
    assert!(gate.release_owned_sync(&mut owner, all(6)).is_err());
    assert!(gate.take_reservation(r.reservation_id).is_none());
    assert!(
        gate.rollback_finalization(applied.receipt.clone())
            .await
            .is_err()
    );
    gate.commit_finalization(r.reservation_id).await;
    assert_eq!(gate.owned_view(&owner).unwrap(), view);
    assert!(gate.admit(request(token, 95)).await.is_err());
    assert!(gate.admit(request(token, 94)).await.is_ok());
}

#[tokio::test]
async fn rollback_refunds_only_exact_applied_amount_once() {
    let (gate, _, _, token) = fixture(15).await;
    let r = gate.admit(request(token, 10)).await.unwrap();
    let mut owner = gate.claim_owned(&r).unwrap();
    gate.finalize_owned_sync(&mut owner, all(20), all(20))
        .unwrap();
    let view = gate.rollback_owned_sync(&mut owner).unwrap();
    assert_eq!(view.status, OwnedBudgetStatus::RolledBack);
    let refund = view.refund.as_ref().unwrap();
    assert_eq!(refund.requested_credit, all(15));
    assert_eq!(refund.applied_credit, all(15));
    assert_eq!(refund.remaining_credit, all(0));
    assert_eq!(gate.rollback_owned_sync(&mut owner).unwrap(), view);
    assert!(gate.release_owned_sync(&mut owner, all(20)).is_err());
    assert!(
        gate.finalize_owned_sync(&mut owner, all(20), all(20))
            .is_err()
    );
    assert!(gate.pending_owned(r.reservation_id).is_none());
    assert!(gate.admit(request(token, 16)).await.is_err());
    assert!(gate.admit(request(token, 15)).await.is_ok());
}

#[tokio::test]
async fn overrun_preserves_known_requested_applied_and_shortfall_on_all_axes() {
    let (gate, _, _, token) = fixture(15).await;
    let r = gate.admit(request(token, 10)).await.unwrap();
    let mut owner = gate.claim_owned(&r).unwrap();
    let view = gate
        .finalize_owned_sync(&mut owner, all(23), all(20))
        .unwrap();
    assert_eq!(view.attempt.unwrap().known_incurred, all(23));
    assert_eq!(view.attempt.unwrap().requested_debit, all(20));
    let application = view.application.unwrap();
    assert_eq!(application.reserved_credit, all(0));
    assert_eq!(application.additional_debit, all(5));
    assert_eq!(application.applied_debit, all(15));
    assert_eq!(application.shortfall, all(5));
    assert_eq!(application.balance_before, all(5));
    assert_eq!(application.balance_after, all(0));
}

#[tokio::test]
async fn retirement_keeps_debit_and_removes_live_state_idempotently() {
    let (gate, _, _, token) = fixture(100).await;
    let r = gate.admit(request(token, 10)).await.unwrap();
    let mut owner = gate.claim_owned(&r).unwrap();
    assert!(gate.commit_owned_sync(&mut owner).is_err());
    gate.finalize_owned_sync(&mut owner, all(6), all(6))
        .unwrap();
    let committed = gate.commit_owned_sync(&mut owner).unwrap();
    assert_eq!(committed.status, OwnedBudgetStatus::Committed);
    assert_eq!(gate.commit_owned_sync(&mut owner).unwrap(), committed);
    assert!(gate.pending_owned(r.reservation_id).is_none());
    assert!(gate.rollback_owned_sync(&mut owner).is_err());
    assert!(gate.release_owned_sync(&mut owner, all(6)).is_err());
    assert!(
        gate.finalize_owned_sync(&mut owner, all(6), all(6))
            .is_err()
    );
    assert!(gate.claim_owned(&r).is_err());
    assert!(gate.take_reservation(r.reservation_id).is_none());
    assert!(gate.admit(request(token, 95)).await.is_err());
    assert!(gate.admit(request(token, 94)).await.is_ok());
}

#[tokio::test]
async fn unrepresentable_request_retains_components_and_usable_owner() {
    for extreme in [i64::MAX as u64 + 1, u64::MAX] {
        for (axis, dimension) in [
            "tokens_in",
            "tokens_out",
            "cents",
            "wall_ms",
            "attention_score",
        ]
        .into_iter()
        .enumerate()
        {
            let (gate, store, holder, token) = fault_fixture(u64::MAX).await;
            let r = gate.admit(request(token, 10)).await.unwrap();
            let mut owner = gate.claim_owned(&r).unwrap();
            let mut requested = all(4);
            match axis {
                0 => requested.tokens_in = extreme,
                1 => requested.tokens_out = extreme,
                2 => requested.cents = extreme,
                3 => requested.wall_ms = extreme,
                _ => requested.attention_score = extreme,
            }
            let error = gate
                .finalize_owned_sync(&mut owner, all(u64::MAX), requested)
                .expect_err("unrepresentable axis must be refused");
            let AdmissionError::Internal(cause) = error else {
                panic!("unrepresentable axis {dimension} must return an internal typed cause");
            };
            let typed = cause
                .downcast_ref::<OwnedDebitUnrepresentable>()
                .expect("exact owned representability cause must survive the gate boundary");
            assert_eq!(typed.dimension, dimension);
            assert_eq!(store.sync_calls.load(Ordering::SeqCst), 0);
            assert_eq!(
                store.current_balance(&holder).await.unwrap(),
                all(u64::MAX - 10)
            );
            let pending = gate.owned_view(&owner).unwrap();
            assert_eq!(pending.status, OwnedBudgetStatus::Active);
            assert_eq!(pending.attempt.unwrap().known_incurred, all(u64::MAX));
            assert_eq!(pending.attempt.unwrap().requested_debit, requested);
            assert!(pending.application.is_none());
            let released = gate.release_owned_sync(&mut owner, all(u64::MAX)).unwrap();
            assert_eq!(released.refund.unwrap().applied_credit, all(10));
            assert_eq!(store.sync_calls.load(Ordering::SeqCst), 1);
            assert_eq!(store.current_balance(&holder).await.unwrap(), all(u64::MAX));
        }
    }
    // Maximum supported nominal debit remains exact, including its rollback.
    let (gate, _, _, token) = fixture(u64::MAX).await;
    let r = gate.admit(request(token, u32::MAX)).await.unwrap();
    let mut owner = gate.claim_owned(&r).unwrap();
    let maximum = all(i64::MAX as u64);
    let view = gate
        .finalize_owned_sync(&mut owner, maximum, maximum)
        .unwrap();
    assert_eq!(view.application.unwrap().applied_debit, maximum);
    assert_eq!(
        gate.rollback_owned_sync(&mut owner)
            .unwrap()
            .refund
            .unwrap()
            .applied_credit,
        maximum
    );
}

#[tokio::test]
async fn failing_sync_refund_retains_pending_owner_and_retries_exactly_once() {
    for rollback in [false, true] {
        let (gate, store, holder, token) = fault_fixture(15).await;
        let r = gate.admit(request(token, 10)).await.unwrap();
        let mut owner = gate.claim_owned(&r).unwrap();
        if rollback {
            gate.finalize_owned_sync(&mut owner, all(20), all(20))
                .unwrap();
        }
        let before = gate.owned_view(&owner).unwrap();
        let balance = store.current_balance(&holder).await.unwrap();
        store.fail_next.store(true, Ordering::SeqCst);
        let failure = if rollback {
            gate.rollback_owned_sync(&mut owner)
        } else {
            gate.release_owned_sync(&mut owner, all(20))
        };
        assert!(failure.is_err());
        assert_eq!(store.current_balance(&holder).await.unwrap(), balance);
        let pending = gate.owned_view(&owner).unwrap();
        assert_eq!(
            pending.status,
            if rollback {
                OwnedBudgetStatus::RollbackPending
            } else {
                OwnedBudgetStatus::ReleasePending
            }
        );
        assert_eq!(pending.application, before.application);
        let requested = if rollback { all(15) } else { all(10) };
        assert_eq!(pending.refund.as_ref().unwrap().remaining_credit, requested);
        assert_eq!(pending.refund.as_ref().unwrap().applied_credit, all(0));
        assert!(gate.commit_owned_sync(&mut owner).is_err());
        assert!(
            gate.finalize_owned_sync(&mut owner, all(0), all(0))
                .is_err()
        );
        let result = if rollback {
            gate.rollback_owned_sync(&mut owner)
        } else {
            gate.release_owned_sync(&mut owner, all(20))
        }
        .unwrap();
        assert_eq!(result.refund.as_ref().unwrap().applied_credit, requested);
        assert_eq!(store.current_balance(&holder).await.unwrap(), all(15));
        let calls = store.sync_calls.load(Ordering::SeqCst);
        let again = if rollback {
            gate.rollback_owned_sync(&mut owner)
        } else {
            gate.release_owned_sync(&mut owner, all(20))
        }
        .unwrap();
        assert_eq!(again, result);
        assert_eq!(store.sync_calls.load(Ordering::SeqCst), calls);
        assert!(gate.pending_owned(r.reservation_id).is_none());
    }
}

#[tokio::test]
async fn clamped_refund_retains_only_unapplied_credit_for_retry() {
    for rollback in [false, true] {
        let (gate, store, holder, token) = fault_fixture(15).await;
        let r = gate.admit(request(token, 10)).await.unwrap();
        let mut owner = gate.claim_owned(&r).unwrap();
        if rollback {
            gate.finalize_owned_sync(&mut owner, all(20), all(20))
                .unwrap();
        }
        let balance = store.current_balance(&holder).await.unwrap();
        let top_up = all(u64::MAX - 5).checked_sub(&balance).unwrap();
        gate.provision_for(&holder, top_up).await.unwrap();
        let partial = if rollback {
            gate.rollback_owned_sync(&mut owner)
        } else {
            gate.release_owned_sync(&mut owner, all(20))
        }
        .unwrap();
        assert_eq!(
            partial.status,
            if rollback {
                OwnedBudgetStatus::RollbackPending
            } else {
                OwnedBudgetStatus::ReleasePending
            }
        );
        assert_eq!(partial.refund.as_ref().unwrap().applied_credit, all(5));
        let remaining = if rollback { 10 } else { 5 };
        assert_eq!(
            partial.refund.as_ref().unwrap().remaining_credit,
            all(remaining)
        );
        let again = if rollback {
            gate.rollback_owned_sync(&mut owner)
        } else {
            gate.release_owned_sync(&mut owner, all(20))
        }
        .unwrap();
        assert_eq!(
            again, partial,
            "no headroom is not a successful full refund"
        );
        assert!(gate.commit_owned_sync(&mut owner).is_err());
        store
            .try_reserve(&holder, &envelope(remaining as u32))
            .await
            .unwrap();
        let done = if rollback {
            gate.rollback_owned_sync(&mut owner)
        } else {
            gate.release_owned_sync(&mut owner, all(20))
        }
        .unwrap();
        assert_eq!(
            done.refund.as_ref().unwrap().applied_credit,
            all(remaining + 5)
        );
        assert_eq!(done.refund.as_ref().unwrap().remaining_credit, all(0));
        assert_eq!(
            done.status,
            if rollback {
                OwnedBudgetStatus::RolledBack
            } else {
                OwnedBudgetStatus::Released
            }
        );
        assert_eq!(store.current_balance(&holder).await.unwrap(), all(u64::MAX));
        assert!(gate.pending_owned(r.reservation_id).is_none());
    }
}

#[tokio::test]
async fn owned_live_lease_ignores_manual_clock_ttl_but_unclaimed_still_expires() {
    let (gate, clock, _, token) = fixture(100).await;
    let owned = gate.admit(request(token, 10)).await.unwrap();
    let ordinary = gate.admit(request(token, 10)).await.unwrap();
    let mut owner = gate.claim_owned(&owned).unwrap();
    clock.advance(1_000_000);
    assert!(matches!(
        gate.claim_owned(&ordinary),
        Err(AdmissionError::ReservationExpired)
    ));
    assert!(matches!(
        gate.finalize(ordinary, all(6)).await,
        Err(AdmissionError::ReservationExpired)
    ));
    let view = gate
        .finalize_owned_sync(&mut owner, all(6), all(6))
        .unwrap();
    assert_eq!(view.status, OwnedBudgetStatus::Finalized);
    assert_eq!(view.application.as_ref().unwrap().balance_before, all(90));
    assert_eq!(view.application.as_ref().unwrap().balance_after, all(94));
    clock.advance(1_000_000);
    assert_eq!(
        gate.rollback_owned_sync(&mut owner)
            .unwrap()
            .refund
            .unwrap()
            .applied_credit,
        all(6)
    );
    assert!(gate.admit(request(token, 101)).await.is_err());
    assert!(gate.admit(request(token, 100)).await.is_ok());
}

#[tokio::test]
async fn concurrent_holders_keep_atomic_application_attribution() {
    let (gate, store, holder, token) = fault_fixture(30).await;
    let other_holder = HolderId("other-holder".into());
    let other_token = TokenId(Uuid::new_v4());
    gate.bind_token(other_token, other_holder.clone());
    gate.provision_for(&other_holder, all(100)).await.unwrap();
    let gate = Arc::new(gate);
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let mut threads = Vec::new();
    for token in [token, token, other_token] {
        let r = gate.admit(request(token, 10)).await.unwrap();
        let mut owner = gate.claim_owned(&r).unwrap();
        let gate = gate.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            let view = gate
                .finalize_owned_sync(&mut owner, all(20), all(20))
                .unwrap();
            (owner, view)
        }));
    }
    barrier.wait();
    let mut same_holder = Vec::new();
    let mut total = all(0);
    for thread in threads {
        let (mut owner, view) = thread.join().unwrap();
        let app = view.application.as_ref().unwrap();
        if view.holder == holder {
            total = total.checked_add(&app.applied_debit).unwrap();
            same_holder.push((
                app.balance_before.cents,
                app.balance_after.cents,
                app.applied_debit.cents,
                app.shortfall.cents,
            ));
            assert_eq!(app.balance_before, all(app.balance_before.cents));
            assert_eq!(app.balance_after, all(app.balance_after.cents));
            assert_eq!(app.applied_debit, all(app.applied_debit.cents));
            assert_eq!(app.shortfall, all(app.shortfall.cents));
        } else {
            assert_eq!(view.holder, other_holder);
            assert_eq!(app.balance_before, all(90));
            assert_eq!(app.balance_after, all(80));
            assert_eq!(app.applied_debit, all(20));
            assert_eq!(app.shortfall, all(0));
        }
        gate.commit_owned_sync(&mut owner).unwrap();
        assert!(gate.pending_owned(view.reservation_id).is_none());
    }
    same_holder.sort_unstable();
    assert_eq!(same_holder, vec![(0, 0, 10, 10), (10, 0, 20, 0)]);
    assert_eq!(total, all(30));
    assert_eq!(store.current_balance(&holder).await.unwrap(), all(0));
    assert_eq!(store.current_balance(&other_holder).await.unwrap(), all(80));
}

#[tokio::test]
async fn mixed_axes_include_clamped_reserved_credit_not_nominal_credit() {
    let (gate, store, holder, token) = fault_fixture(20).await;
    let r = gate.admit(request(token, 10)).await.unwrap();
    let mut owner = gate.claim_owned(&r).unwrap();
    let before = CostTuple {
        tokens_in: u64::MAX - 2,
        tokens_out: 5,
        cents: 1,
        wall_ms: u64::MAX,
        attention_score: 50,
    };
    let add = CostTuple {
        tokens_in: u64::MAX - 12,
        tokens_out: 0,
        cents: 0,
        wall_ms: u64::MAX - 10,
        attention_score: 40,
    };
    gate.provision_for(&holder, add).await.unwrap();
    // Other live reservations consume capacity on just two axes.
    store
        .try_reserve(
            &holder,
            &CostEnvelope {
                tokens_out_max: 5,
                cents_max: 9,
                ..CostEnvelope::default()
            },
        )
        .await
        .unwrap();
    let requested = CostTuple {
        tokens_in: 1,
        tokens_out: 30,
        cents: 20,
        wall_ms: 0,
        attention_score: 12,
    };
    let view = gate
        .finalize_owned_sync(&mut owner, requested, requested)
        .unwrap();
    let app = view.application.unwrap();
    assert_eq!(app.balance_before, before);
    assert_eq!(
        app.reserved_credit,
        CostTuple {
            tokens_in: 2,
            ..CostTuple::ZERO
        }
    );
    assert_eq!(
        app.additional_debit,
        CostTuple {
            tokens_out: 5,
            cents: 1,
            attention_score: 2,
            ..CostTuple::ZERO
        }
    );
    assert_eq!(
        app.applied_debit,
        CostTuple {
            tokens_in: 8,
            tokens_out: 15,
            cents: 11,
            wall_ms: 10,
            attention_score: 12
        }
    );
    assert_eq!(
        app.shortfall,
        CostTuple {
            tokens_out: 15,
            cents: 9,
            ..CostTuple::ZERO
        }
    );
    assert_eq!(
        app.balance_after,
        store.current_balance(&holder).await.unwrap()
    );
}

#[tokio::test]
async fn failed_finalization_preserves_attempt_and_live_owner_for_retry() {
    let (gate, store, holder, token) = fault_fixture(100).await;
    let r = gate.admit(request(token, 10)).await.unwrap();
    let mut owner = gate.claim_owned(&r).unwrap();
    store.fail_next.store(true, Ordering::SeqCst);
    assert!(
        gate.finalize_owned_sync(&mut owner, all(23), all(20))
            .is_err()
    );
    assert_eq!(store.current_balance(&holder).await.unwrap(), all(90));
    let pending = gate.owned_view(&owner).unwrap();
    assert_eq!(pending.status, OwnedBudgetStatus::Active);
    assert_eq!(
        pending.attempt.unwrap(),
        OwnedCost {
            known_incurred: all(23),
            requested_debit: all(20)
        }
    );
    assert!(pending.application.is_none());
    assert!(gate.take_reservation(r.reservation_id).is_none());
    assert!(gate.finalize(r, all(20)).await.is_err());
    let view = gate
        .finalize_owned_sync(&mut owner, all(23), all(20))
        .unwrap();
    assert_eq!(view.application.unwrap().applied_debit, all(20));
    assert_eq!(store.current_balance(&holder).await.unwrap(), all(80));
    assert_eq!(store.sync_calls.load(Ordering::SeqCst), 2);
    assert!(
        gate.finalize_owned_sync(&mut owner, all(23), all(20))
            .is_err()
    );
    assert_eq!(store.sync_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn release_retry_cannot_silently_replace_known_evidence() {
    let (gate, store, _, token) = fault_fixture(100).await;
    let r = gate.admit(request(token, 10)).await.unwrap();
    let mut owner = gate.claim_owned(&r).unwrap();
    store.fail_next.store(true, Ordering::SeqCst);
    assert!(gate.release_owned_sync(&mut owner, all(7)).is_err());
    assert!(gate.release_owned_sync(&mut owner, all(8)).is_err());
    assert_eq!(store.sync_calls.load(Ordering::SeqCst), 1);
    let done = gate.release_owned_sync(&mut owner, all(7)).unwrap();
    assert_eq!(done.attempt.unwrap().known_incurred, all(7));
    assert!(gate.release_owned_sync(&mut owner, all(8)).is_err());
    assert_eq!(gate.release_owned_sync(&mut owner, all(7)).unwrap(), done);
}

fn all(value: u64) -> CostTuple {
    CostTuple {
        tokens_in: value,
        tokens_out: value,
        cents: value,
        wall_ms: value,
        attention_score: value,
    }
}

fn envelope(value: u32) -> CostEnvelope {
    CostEnvelope {
        tokens_in_max: value,
        tokens_out_max: value,
        cents_max: value,
        wall_ms_max: value,
        attention_score_max: value,
    }
}

#[tokio::test]
async fn sync_refund_returns_atomic_actual_balances_on_every_axis() {
    let store = InMemoryBudgetStore::new();
    let holder = HolderId("owner".into());
    store.set_balance(holder.clone(), all(15));
    let handle = store.try_reserve(&holder, &envelope(10)).await.unwrap();
    let (before, after) = store
        .refund_sync_with_balances(handle, CostDelta::between(&all(10), &all(20)))
        .unwrap();
    assert_eq!(before, all(5));
    assert_eq!(after, all(0));
    assert_eq!(store.current_balance(&holder).await.unwrap(), after);
}
