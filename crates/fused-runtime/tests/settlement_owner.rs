//! Real concrete budget and local durable-store owner acceptance.
use ardur_cost_gate::*;
use ardur_fused_runtime::{SharedBudget, settlement::*};
use ardur_session_journals::{SessionId, settlement::*};
use std::sync::Arc;
use uuid::Uuid;

fn identity(root: &std::path::Path) -> ReceiptIdentity {
    ReceiptIdentity {
        receipt_log: root.join("receipts.jsonl"),
        signer: Sha256Digest::of(b"public fixture identity"),
    }
}
fn request(token: TokenId) -> AdmissionRequest {
    AdmissionRequest {
        cap_token_id: token,
        projected_envelope: CostEnvelope {
            cents_max: 10,
            ..Default::default()
        },
        provider_id: ProviderId("provider".into()),
        model_id: ModelId("model".into()),
        request_digest: Sha256Digest::of(b"request"),
    }
}
fn observation(cost: CostTuple) -> ProviderEvidence {
    ProviderEvidence::Observed {
        usage: Some(UsageSnapshot {
            input_tokens: 1,
            output_tokens: 1,
        }),
        cost,
        provenance: CostProvenance::ReportedCost,
        finished: false,
        interrupted: false,
    }
}
fn attribution(token: TokenId, holder: &HolderId, journal: bool) -> TurnIdentity {
    TurnIdentity {
        request_session: SessionId(Uuid::new_v4()),
        journal_owner: journal.then(|| SessionId(Uuid::new_v4())),
        verified_subject: HolderId("different-verified-subject".into()),
        budget_holder: holder.clone(),
        cap_token_id: token,
        started_at: UnixTsMillis(123),
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    root: std::path::PathBuf,
    receipt: ReceiptIdentity,
    budget: SharedBudget,
    holder: HolderId,
    token: TokenId,
    gate: Arc<InMemoryCostAdmissionGate<SharedBudget>>,
    clock: Arc<ManualClock>,
    coordinator: Arc<SettlementCoordinator>,
}
impl Fixture {
    fn new(balance: CostTuple) -> Self {
        Self::with_history(balance, None)
    }
    fn with_history(balance: CostTuple, ordinal: Option<u64>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().canonicalize().unwrap();
        let root = parent.join("store");
        let receipt = identity(&parent);
        if let Some(ordinal) = ordinal {
            seed_terminal_history(&root, &receipt, ordinal);
        }
        let budget = SharedBudget::new();
        let holder = HolderId("real-holder".into());
        let token = TokenId(Uuid::new_v4());
        budget.set_balance(holder.clone(), balance);
        let clock = Arc::new(ManualClock::new(UnixTsMillis(123)));
        let gate = Arc::new(InMemoryCostAdmissionGate::with_clock(
            budget.clone(),
            clock.clone(),
        ));
        gate.bind_token(token, holder.clone());
        let coordinator =
            SettlementCoordinator::open(gate.clone(), &root, receipt.clone()).unwrap();
        Self {
            _temp: temp,
            root,
            receipt,
            budget,
            holder,
            token,
            gate,
            clock,
            coordinator,
        }
    }
    async fn owner(&self, journal: bool) -> TurnSettlementOwner {
        let mut owner = self
            .coordinator
            .reserve_turn(attribution(self.token, &self.holder, journal))
            .unwrap();
        owner
            .admit_round(request(self.token), Uuid::new_v4())
            .await
            .unwrap();
        owner
    }
    fn saved(&self) -> Vec<EncodedSnapshot> {
        load_settlement_snapshot(&self.root, &self.receipt, &LIMITS)
            .unwrap()
            .snapshots
    }
}
// Public retained-history fixtures: no live-state mutation or fabricated JSON.
fn seed_terminal_history(root: &std::path::Path, receipt: &ReceiptIdentity, ordinal: u64) {
    let epoch = BudgetEpoch(Uuid::new_v4());
    let encoded = EncodedSnapshot::new(
        1,
        TurnObligation {
            schema_version: SETTLEMENT_SCHEMA_VERSION,
            turn_id: TurnId(Uuid::new_v4()),
            budget_epoch: epoch,
            request_session: SessionId(Uuid::new_v4()),
            journal_owner: None,
            verified_subject: HolderId("historical-subject".into()),
            budget_holder: HolderId("historical-holder".into()),
            cap_token_id: TokenId(Uuid::new_v4()),
            started_at: UnixTsMillis(1),
            rounds: vec![RoundObligation {
                settlement_id: SettlementId(Uuid::new_v4()),
                ordinal: 0,
                provider_request_id: Uuid::new_v4(),
                provider: "provider".into(),
                model: "model".into(),
                request_digest: Sha256Digest::of(b"history"),
                reserved: CostTuple::ZERO,
                provider_evidence: ProviderEvidence::NotDispatched,
                tools: vec![],
                known_incurred: CostTuple::ZERO,
                decision: Some(SettlementDecision::Cancelled),
                phase: SettlementPhase::Settled {
                    application: DebitApplication {
                        epoch,
                        requested_debit: CostTuple::ZERO,
                        applied_debit: CostTuple::ZERO,
                        reserved_credit: CostTuple::ZERO,
                        additional_debit: CostTuple::ZERO,
                        shortfall: CostTuple::ZERO,
                        rollback: RollbackStatus::None,
                    },
                    receipt: None,
                },
                commit_ordinal: Some(ordinal),
                projection: JournalProjection::NotConfigured,
            }],
            terminal: TurnTerminal::Cancelled,
            cancellation_marker: MarkerProjection::NotRequired,
        },
        &LIMITS,
    )
    .unwrap();
    let mut writer = FileSettlementStore::open(root, receipt.clone(), LIMITS).unwrap();
    assert_eq!(
        writer.put_exact(None, &encoded),
        WriteResolution::Durable(1)
    );
    assert_eq!(writer.health(), StoreHealth::Healthy);
    assert_eq!(
        std::fs::read(root.join(format!("{}.json", encoded.turn().turn_id.0))).unwrap(),
        encoded.bytes()
    );
    drop(writer);
    let disk = load_settlement_snapshot(root, receipt, &LIMITS).unwrap();
    assert_eq!(disk.snapshots.len(), 1);
    assert_eq!(disk.snapshots[0].bytes(), encoded.bytes());
}
async fn exhausted_owner_or_refusal(f: &Fixture) -> Option<TurnSettlementOwner> {
    let before = f.saved()[0].clone();
    assert_eq!(before.turn().rounds[0].commit_ordinal, Some(u64::MAX - 1));
    assert_eq!(f.coordinator.status().boot_problem, None);
    assert_eq!(f.coordinator.status().inventory_count, 1);
    assert_eq!(f.budget.current_balance(&f.holder).await.unwrap(), all(30));
    let refusal = match f
        .coordinator
        .reserve_turn(attribution(f.token, &f.holder, false))
    {
        Ok(mut owner) => match owner
            .admit_round(all_request(f.token, 10), Uuid::new_v4())
            .await
        {
            Ok(()) => return Some(owner),
            Err(error) => {
                let state = f.coordinator.status();
                assert!(state.turns[0].admission_attempt.is_none());
                assert!(state.turns[0].view.is_none());
                assert!(state.turns[0].unclaimed_reservation.is_none());
                assert!(state.turns[0].turn.rounds.is_empty());
                owner.close_empty().unwrap();
                error
            }
        },
        Err(error) => error,
    };
    assert!(matches!(
        refusal,
        SettlementError::Invalid(SettlementProblem::Bounds)
    ));
    assert_eq!(f.budget.current_balance(&f.holder).await.unwrap(), all(30));
    assert!(f.coordinator.status().turns.is_empty());
    assert_eq!(f.coordinator.status().executing, None);
    assert_eq!(f.coordinator.status().inventory_count, 1);
    assert_eq!(f.saved().len(), 1);
    assert_eq!(f.saved()[0].bytes(), before.bytes());
    // Repeat must not reopen capacity or erase history.
    assert!(matches!(
        f.coordinator
            .reserve_turn(attribution(f.token, &f.holder, false)),
        Err(SettlementError::Invalid(SettlementProblem::Bounds))
    ));
    None
}
#[tokio::test]
async fn retained_ordinal_exhaustion_cannot_finalize_failed_preparation() {
    let f = Fixture::with_history(all(30), Some(u64::MAX - 1));
    let Some(mut owner) = exhausted_owner_or_refusal(&f).await else {
        return;
    };
    let id = owner.turn_id();
    owner.observe_provider(observation(all(9))).unwrap();
    let disk = f
        .saved()
        .into_iter()
        .find(|s| s.turn().turn_id == id)
        .unwrap();
    assert!(matches!(
        disk.turn().rounds[0].phase,
        SettlementPhase::WorkObserved
    ));
    assert!(matches!(
        owner.prepare(Disposition::Refusal(RefusalClass::Authorization), all(9)),
        Err(SettlementError::Invalid(SettlementProblem::Bounds))
    ));
    let held = f.budget.current_balance(&f.holder).await.unwrap();
    assert_eq!(held, all(20));
    let finalized = owner.finalize_sync();
    let after = f.coordinator.status();
    println!("FAILED_PREPARE_FINALIZE={finalized:?} STATUS={after:?}");
    assert_eq!(
        f.budget.current_balance(&f.holder).await.unwrap(),
        held,
        "failed preparation must not permit economic finalization"
    );
    assert!(matches!(
        finalized,
        Err(SettlementError::AdmissionClosed | SettlementError::State)
    ));
    assert_eq!(after.turns[0].turn, *disk.turn());
    assert!(!after.turns[0].undurable);
    assert!(matches!(
        after.turns[0].error,
        Some(SettlementError::Invalid(SettlementProblem::Bounds))
    ));
    assert_eq!(
        after.turns[0].view.as_ref().unwrap().status,
        OwnedBudgetStatus::Active
    );
    assert!(after.turns[0].view.as_ref().unwrap().attempt.is_none());
    assert_eq!(
        f.saved()
            .into_iter()
            .find(|s| s.turn().turn_id == id)
            .unwrap()
            .bytes(),
        disk.bytes()
    );
    drop(owner);
    assert!(matches!(
        f.coordinator
            .reserve_turn(attribution(f.token, &f.holder, false)),
        Err(SettlementError::AdmissionClosed)
    ));
    assert!(f.coordinator.supervisor().try_close().is_err());
}
#[tokio::test]
async fn retained_ordinal_exhaustion_cannot_report_cancelled_active_hold() {
    let f = Fixture::with_history(all(30), Some(u64::MAX - 1));
    let Some(mut owner) = exhausted_owner_or_refusal(&f).await else {
        return;
    };
    let id = owner.turn_id();
    let disk = f
        .saved()
        .into_iter()
        .find(|s| s.turn().turn_id == id)
        .unwrap();
    let hold = disk.turn().rounds[0].settlement_id.0;
    assert!(matches!(
        owner.abandon_sync(),
        Err(SettlementError::Invalid(SettlementProblem::Bounds))
    ));
    let second = owner.abandon_sync();
    drop(owner);
    let status = f.coordinator.status();
    let admission = f
        .coordinator
        .reserve_turn(attribution(f.token, &f.holder, false));
    println!(
        "REPEAT_CANCEL={second:?} AFTER_DROP={status:?} ADMISSION_OK={}",
        admission.is_ok()
    );
    assert!(
        matches!(
            second,
            Err(SettlementError::AdmissionClosed
                | SettlementError::Invalid(SettlementProblem::Bounds))
        ),
        "repeat cancellation must not report success for an Active unreleased hold"
    );
    assert_eq!(f.budget.current_balance(&f.holder).await.unwrap(), all(20));
    assert_eq!(
        f.gate.pending_owned(hold).unwrap().status,
        OwnedBudgetStatus::Active
    );
    assert!(f.gate.pending_owned(hold).unwrap().refund.is_none());
    assert_eq!(status.turns[0].turn, *disk.turn());
    assert!(matches!(
        status.turns[0].error,
        Some(SettlementError::Invalid(SettlementProblem::Bounds))
    ));
    assert!(!status.turns[0].undurable);
    assert!(
        status.turns[0].abandonment_pending,
        "a decision alone is not a cancellation continuation"
    );
    assert!(matches!(admission, Err(SettlementError::AdmissionClosed)));
    assert!(matches!(
        f.coordinator.supervisor().retry_abandonment(id),
        Err(SettlementError::AdmissionClosed)
    ));
    assert_eq!(
        f.saved()
            .into_iter()
            .find(|s| s.turn().turn_id == id)
            .unwrap()
            .bytes(),
        disk.bytes()
    );
    assert!(f.coordinator.supervisor().try_close().is_err());
}
#[tokio::test]
async fn retained_exhausted_capacity_is_refused_before_economic_admission() {
    let f = Fixture::with_history(all(30), Some(u64::MAX - 1));
    assert!(
        exhausted_owner_or_refusal(&f).await.is_none(),
        "exhausted projection capacity must be refused before a real hold"
    );
}
#[tokio::test]
async fn retained_last_usable_ordinal_settles_then_refuses_next_turn() {
    for paid in [false, true] {
        let f = Fixture::with_history(all(30), Some(u64::MAX - 2));
        let history = f.saved()[0].clone();
        let mut owner = f
            .coordinator
            .reserve_turn(attribution(f.token, &f.holder, false))
            .unwrap();
        owner
            .admit_round(all_request(f.token, 10), Uuid::new_v4())
            .await
            .unwrap();
        let id = owner.turn_id();
        if paid {
            owner.observe_provider(observation(all(9))).unwrap();
            owner
                .prepare(Disposition::Refusal(RefusalClass::Authorization), all(9))
                .unwrap();
            owner.finalize_sync().unwrap();
            owner.commit_disposition().unwrap();
        } else {
            owner.abandon_sync().unwrap();
            let first = f
                .saved()
                .into_iter()
                .find(|s| s.turn().turn_id == id)
                .unwrap();
            owner.abandon_sync().unwrap();
            assert_eq!(
                first.bytes(),
                f.saved()
                    .into_iter()
                    .find(|s| s.turn().turn_id == id)
                    .unwrap()
                    .bytes()
            );
        }
        drop(owner);
        let disk = f.saved();
        let last = disk.iter().find(|s| s.turn().turn_id == id).unwrap();
        assert_eq!(last.turn().rounds[0].commit_ordinal, Some(u64::MAX - 1));
        assert!(matches!(
            last.turn().rounds[0].phase,
            SettlementPhase::Settled { .. }
        ));
        assert_eq!(
            disk.iter()
                .find(|s| s.turn().turn_id == history.turn().turn_id)
                .unwrap()
                .bytes(),
            history.bytes()
        );
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            all(if paid { 21 } else { 30 })
        );
        assert!(f.coordinator.status().turns.is_empty());
        assert!(
            matches!(
                f.coordinator
                    .reserve_turn(attribution(f.token, &f.holder, false)),
                Err(SettlementError::Invalid(SettlementProblem::Bounds))
            ),
            "no wrap or ordinal reuse after last usable ordinal"
        );
        drop(f.coordinator);
        let reopened =
            SettlementCoordinator::open(f.gate.clone(), &f.root, f.receipt.clone()).unwrap();
        assert!(matches!(
            reopened.reserve_turn(attribution(f.token, &f.holder, false)),
            Err(SettlementError::Invalid(SettlementProblem::Bounds))
        ));
    }
}
#[tokio::test]
async fn retained_max_ordinal_open_fails_without_budget_or_history_changes() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().canonicalize().unwrap();
    let root = parent.join("store");
    let receipt = identity(&parent);
    seed_terminal_history(&root, &receipt, u64::MAX);
    let before = load_settlement_snapshot(&root, &receipt, &LIMITS)
        .unwrap()
        .snapshots
        .remove(0);
    let budget = SharedBudget::new();
    let holder = HolderId("holder".into());
    budget.set_balance(holder.clone(), all(30));
    let gate = Arc::new(InMemoryCostAdmissionGate::new(budget.clone()));
    assert!(matches!(
        SettlementCoordinator::open(gate, &root, receipt.clone()),
        Err(SettlementError::Invalid(SettlementProblem::Bounds))
    ));
    assert_eq!(budget.current_balance(&holder).await.unwrap(), all(30));
    assert_eq!(
        load_settlement_snapshot(&root, &receipt, &LIMITS)
            .unwrap()
            .snapshots[0]
            .bytes(),
        before.bytes()
    );
}
#[tokio::test]
async fn retained_ordinary_ordinal_prepared_facts_debit_once() {
    let f = Fixture::with_history(all(30), Some(7));
    let mut owner = f
        .coordinator
        .reserve_turn(attribution(f.token, &f.holder, false))
        .unwrap();
    owner
        .admit_round(all_request(f.token, 10), Uuid::new_v4())
        .await
        .unwrap();
    let id = owner.turn_id();
    owner.observe_provider(observation(all(9))).unwrap();
    owner
        .prepare(Disposition::Refusal(RefusalClass::Authorization), all(9))
        .unwrap();
    let prepared = f
        .saved()
        .into_iter()
        .find(|s| s.turn().turn_id == id)
        .unwrap();
    assert_eq!(prepared.turn(), &f.coordinator.status().turns[0].turn);
    assert!(matches!(
        prepared.turn().rounds[0].phase,
        SettlementPhase::Prepared { candidate: None }
    ));
    assert_eq!(prepared.turn().rounds[0].commit_ordinal, Some(8));
    owner.finalize_sync().unwrap();
    assert_eq!(f.budget.current_balance(&f.holder).await.unwrap(), all(21));
    assert!(matches!(owner.finalize_sync(), Err(SettlementError::State)));
    owner.commit_disposition().unwrap();
    drop(owner);
    assert_eq!(f.budget.current_balance(&f.holder).await.unwrap(), all(21));
    assert!(f.coordinator.supervisor().try_close().is_ok());
}

#[tokio::test]
async fn drop_releases_owned_hold_and_keeps_pending_projection() {
    let f = Fixture::new(CostTuple::cents(10));
    let mut owner = f.owner(true).await;
    let id = f.saved()[0].turn().rounds[0].settlement_id.0;
    f.clock.advance(1_000_000);
    assert!(
        f.gate.take_reservation(id).is_none(),
        "generic cleanup cannot steal claimed authority"
    );
    owner
        .observe_provider(ProviderEvidence::DispatchIntent)
        .unwrap();
    owner
        .observe_provider(observation(CostTuple::cents(9)))
        .unwrap();
    drop(owner);
    assert_eq!(
        f.budget.current_balance(&f.holder).await.unwrap(),
        CostTuple::cents(10),
        "owning Drop must synchronously release the real hold"
    );
    let saved = f.saved();
    let turn = saved[0].turn();
    let round = &turn.rounds[0];
    assert_eq!(turn.terminal, TurnTerminal::Cancelled);
    assert_eq!(round.projection, JournalProjection::Pending(id));
    assert!(round.commit_ordinal.is_some());
    assert!(matches!(
        round.provider_evidence,
        ProviderEvidence::Observed {
            interrupted: true,
            ..
        }
    ));
    assert!(f.gate.pending_owned(id).is_none());
}
fn all(n: u64) -> CostTuple {
    CostTuple {
        tokens_in: n,
        tokens_out: n,
        cents: n,
        wall_ms: n,
        attention_score: n,
    }
}
fn all_request(token: TokenId, n: u32) -> AdmissionRequest {
    AdmissionRequest {
        projected_envelope: CostEnvelope {
            tokens_in_max: n,
            tokens_out_max: n,
            cents_max: n,
            wall_ms_max: n,
            attention_score_max: n,
        },
        ..request(token)
    }
}
#[tokio::test]
async fn clamped_finalization_retains_authority_instead_of_committing() {
    let f = Fixture::new(all(10));
    let mut owner = f
        .coordinator
        .reserve_turn(attribution(f.token, &f.holder, false))
        .unwrap();
    owner
        .admit_round(all_request(f.token, 10), Uuid::new_v4())
        .await
        .unwrap();
    owner.observe_provider(observation(all(9))).unwrap();
    owner
        .prepare(Disposition::Refusal(RefusalClass::OutputBlocked), all(9))
        .unwrap();
    f.budget
        .provision_merge(&f.holder, &all(u64::MAX), None)
        .await
        .unwrap();
    let result = owner.finalize_sync();
    let saved = f.saved();
    let round = &saved[0].turn().rounds[0];
    assert!(
        matches!(
            round.phase,
            SettlementPhase::Unresolved {
                problem: SettlementProblem::ReleaseCreditPending,
                ..
            }
        ),
        "clamped actual credit is unresolved, not an ordinary finalized outcome"
    );
    assert!(result.is_err());
    assert!(owner.commit_disposition().is_err());
    let view = f.gate.pending_owned(round.settlement_id.0).unwrap();
    let app = view.application.unwrap();
    assert_eq!(app.reserved_credit, all(0));
    assert_eq!(app.applied_debit, all(10));
    assert_eq!(app.shortfall, all(0));
    assert!(matches!(
        f.coordinator
            .reserve_turn(attribution(f.token, &f.holder, false)),
        Err(SettlementError::AdmissionClosed)
    ));
    drop(owner);
    assert_eq!(
        f.budget.current_balance(&f.holder).await.unwrap(),
        all(u64::MAX)
    );
    assert_eq!(
        f.gate.pending_owned(round.settlement_id.0).unwrap().status,
        OwnedBudgetStatus::Finalized,
        "Drop cannot invent excess-only refunds or compensate paid refusal"
    );
}
#[tokio::test]
async fn partial_release_survives_owner_and_coordinator_drop_in_supervisor() {
    let f = Fixture::new(all(10));
    let mut owner = f
        .coordinator
        .reserve_turn(attribution(f.token, &f.holder, false))
        .unwrap();
    owner
        .admit_round(all_request(f.token, 10), Uuid::new_v4())
        .await
        .unwrap();
    owner.observe_provider(observation(all(9))).unwrap();
    let turn = owner.turn_id();
    let supervisor = f.coordinator.supervisor();
    f.budget
        .provision_merge(&f.holder, &all(u64::MAX - 3), None)
        .await
        .unwrap();
    assert!(
        matches!(
            owner.abandon_sync(),
            Err(SettlementError::CreditPending(
                SettlementProblem::ReleaseCreditPending
            ))
        ),
        "partial real release must report pending rather than nominal success"
    );
    assert!(
        matches!(
            owner.abandon_sync(),
            Err(SettlementError::CreditPending(
                SettlementProblem::ReleaseCreditPending
            ))
        ),
        "idempotent abandon must not turn a pending credit into success"
    );
    let status = supervisor.status();
    let view = status.turns[0].view.as_ref().unwrap();
    assert_eq!(view.status, OwnedBudgetStatus::ReleasePending);
    assert_eq!(view.refund.as_ref().unwrap().applied_credit, all(3));
    assert_eq!(view.refund.as_ref().unwrap().remaining_credit, all(7));
    let saved = f.saved();
    let SettlementPhase::Unresolved {
        problem: SettlementProblem::ReleaseCreditPending,
        application: Some(app),
        ..
    } = &saved[0].turn().rounds[0].phase
    else {
        panic!("partial facts must be durable before retry")
    };
    assert_eq!(app.reserved_credit, all(3));
    assert_eq!(app.applied_debit, all(7));
    drop(owner);
    drop(f.coordinator);
    let supervisor = supervisor
        .try_close()
        .expect_err("retained authority prevents closure");
    f.clock.advance(1_000_000);
    f.budget
        .try_reserve(&f.holder, &all_request(f.token, 7).projected_envelope)
        .await
        .unwrap();
    supervisor.retry_refund(turn).unwrap();
    assert_eq!(
        f.budget.current_balance(&f.holder).await.unwrap(),
        all(u64::MAX)
    );
    assert!(
        matches!(supervisor.retry_refund(turn), Err(SettlementError::State)),
        "completed work is not another refund"
    );
    let saved = load_settlement_snapshot(&f.root, &f.receipt, &LIMITS).unwrap();
    let SettlementPhase::Settled {
        application,
        receipt: None,
    } = &saved.snapshots[0].turn().rounds[0].phase
    else {
        panic!("full actual credit closes")
    };
    assert_eq!(application.reserved_credit, all(10));
    assert_eq!(application.applied_debit, all(0));
    assert!(supervisor.try_close().is_ok());
}
#[tokio::test]
async fn explicit_full_rollback_retries_only_actual_remaining_credit() {
    let f = Fixture::new(all(13));
    let mut owner = f
        .coordinator
        .reserve_turn(attribution(f.token, &f.holder, false))
        .unwrap();
    owner
        .admit_round(all_request(f.token, 10), Uuid::new_v4())
        .await
        .unwrap();
    owner.observe_provider(observation(all(20))).unwrap();
    owner
        .prepare(Disposition::Refusal(RefusalClass::Authorization), all(15))
        .unwrap();
    owner.finalize_sync().unwrap();
    let turn = owner.turn_id();
    let supervisor = f.coordinator.supervisor();
    let before = supervisor.status().turns[0]
        .view
        .as_ref()
        .unwrap()
        .application
        .clone()
        .unwrap();
    assert_eq!(before.additional_debit, all(3));
    assert_eq!(before.applied_debit, all(13));
    assert_eq!(before.shortfall, all(2));
    f.budget
        .provision_merge(&f.holder, &all(u64::MAX - 4), None)
        .await
        .unwrap();
    let result = supervisor.compensate_definite_failure(turn);
    assert!(
        matches!(
            result,
            Err(SettlementError::CreditPending(
                SettlementProblem::RollbackCreditPending
            ))
        ),
        "explicit compensation must retain the actual pending rollback"
    );
    let saved = f.saved();
    let SettlementPhase::Unresolved {
        problem: SettlementProblem::RollbackCreditPending,
        application: Some(app),
        ..
    } = &saved[0].turn().rounds[0].phase
    else {
        panic!("pending rollback must be durable")
    };
    assert_eq!(app.applied_debit, all(13));
    assert_eq!(app.requested_debit, all(15));
    assert_eq!(app.rollback, RollbackStatus::Applied(all(4)));
    assert_eq!(
        supervisor.status().turns[0]
            .view
            .as_ref()
            .unwrap()
            .refund
            .as_ref()
            .unwrap()
            .remaining_credit,
        all(9)
    );
    drop(owner);
    f.budget
        .try_reserve(&f.holder, &all_request(f.token, 9).projected_envelope)
        .await
        .unwrap();
    supervisor.retry_refund(turn).unwrap();
    assert_eq!(
        f.budget.current_balance(&f.holder).await.unwrap(),
        all(u64::MAX)
    );
    let saved = f.saved();
    let SettlementPhase::Settled {
        application,
        receipt: None,
    } = &saved[0].turn().rounds[0].phase
    else {
        panic!("full rollback closes without a receipt")
    };
    assert_eq!(application.rollback, RollbackStatus::Applied(all(13)));
    assert_eq!(application.applied_debit, all(13));
    assert_eq!(
        saved[0].turn().rounds[0].decision,
        Some(SettlementDecision::Refusal(RefusalClass::Authorization))
    );
    assert!(supervisor.try_close().is_ok());
}
fn tool(cost: CostTuple) -> ToolEvidence {
    ToolEvidence {
        ordinal: 0,
        call_id: "call".into(),
        name: "tool".into(),
        arguments_digest: Sha256Digest::of(b"arguments"),
        effect: ToolEffect::Completed {
            output_digest: Sha256Digest::of(b"output"),
            cost,
        },
        output_admission: OutputAdmission::Allowed,
    }
}
#[tokio::test]
async fn overflow_component_is_retained_without_zeroing_or_claiming_durability() {
    let f = Fixture::new(CostTuple::cents(10));
    let mut owner = f.owner(false).await;
    owner.observe_provider(observation(all(u64::MAX))).unwrap();
    let rejected = tool(all(1));
    let result = owner.observe_tool(rejected.clone());
    assert!(
        matches!(
            result,
            Err(SettlementError::Invalid(SettlementProblem::Corrupt))
        ),
        "checked overflow must be classified without a nominal total"
    );
    let status = f.coordinator.status();
    assert_eq!(
        status.turns[0].rejected.as_deref(),
        Some(&RejectedObservation::Tool(rejected))
    );
    assert_eq!(status.turns[0].turn.rounds[0].known_incurred, all(u64::MAX));
    assert_eq!(f.saved()[0].turn().rounds[0].tools.len(), 0);
    assert!(matches!(
        owner.observe_provider(observation(all(0))),
        Err(SettlementError::AdmissionClosed)
    ));
    assert!(matches!(
        f.coordinator
            .reserve_turn(attribution(f.token, &f.holder, false)),
        Err(SettlementError::AdmissionClosed)
    ));
    let supervisor = f.coordinator.supervisor();
    drop(owner);
    assert_eq!(
        f.budget.current_balance(&f.holder).await.unwrap(),
        CostTuple::ZERO,
        "quarantine retains authority, not invented disposal"
    );
    assert!(supervisor.try_close().is_err());
}
#[derive(Default)]
struct CallbackClock {
    action: parking_lot::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    calls: std::sync::atomic::AtomicUsize,
}
impl Clock for CallbackClock {
    fn now_ms(&self) -> UnixTsMillis {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let action = self.action.lock().take();
        if let Some(action) = action {
            action();
        }
        UnixTsMillis(123)
    }
}
// Every potentially blocking reentry probe executes in a child supervised by a
// parent deadline, not an async timeout on the thread that could be blocked.
#[tokio::test]
async fn settlement_process_probe() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::Ordering::SeqCst;
    let Ok(mode) = std::env::var("ARDUR_SETTLEMENT_PROBE_MODE") else {
        return;
    };
    println!("PROBE_BEGIN:{mode}");
    if mode == "hang" {
        loop {
            std::thread::park();
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().canonicalize().unwrap();
    let root = parent.join("store");
    let receipt = identity(&parent);
    let budget = SharedBudget::new();
    let holder = HolderId("process-holder".into());
    let token = TokenId(Uuid::new_v4());
    budget.set_balance(holder.clone(), CostTuple::cents(10));
    let clock = Arc::new(CallbackClock::default());
    let gate = Arc::new(InMemoryCostAdmissionGate::with_clock(
        budget.clone(),
        clock.clone(),
    ));
    gate.bind_token(token, holder.clone());
    let coordinator = SettlementCoordinator::open(gate.clone(), &root, receipt.clone()).unwrap();
    let mut owner = coordinator
        .reserve_turn(attribution(token, &holder, false))
        .unwrap();
    owner
        .admit_round(request(token), Uuid::new_v4())
        .await
        .unwrap();
    owner
        .observe_provider(observation(CostTuple::cents(9)))
        .unwrap();
    let id = owner.turn_id();
    let supervisor = coordinator.supervisor();
    let before_calls = clock.calls.load(SeqCst);
    if mode == "drop" {
        *clock.action.lock() = Some(Box::new(|| {
            panic!("Drop must never sample arbitrary clocks")
        }));
        drop(owner);
        assert_eq!(clock.calls.load(SeqCst), before_calls);
        assert_eq!(
            budget.current_balance(&holder).await.unwrap(),
            CostTuple::cents(10)
        );
        assert!(supervisor.try_close().is_ok());
        println!("PROBE_END:{mode}");
        return;
    }
    owner
        .prepare(
            Disposition::Refusal(RefusalClass::Authorization),
            CostTuple::cents(9),
        )
        .unwrap();
    let weak = Arc::downgrade(&coordinator);
    let callback_root = root.clone();
    let callback_holder = holder.clone();
    let callback_mode = mode.clone();
    *clock.action.lock() = Some(Box::new(move || {
        let coordinator = weak.upgrade().unwrap();
        let status = coordinator.status();
        assert_eq!(status.busy, Some(id));
        assert_eq!(
            status.turns[0].view.as_ref().unwrap().status,
            OwnedBudgetStatus::Active
        );
        assert!(matches!(
            coordinator.supervisor().retry_refund(id),
            Err(SettlementError::Busy)
        ));
        assert!(matches!(
            coordinator.reserve_turn(attribution(token, &callback_holder, false)),
            Err(SettlementError::Busy)
        ));
        println!("PROBE_REENTERED:{callback_mode}");
        if callback_mode == "panic" {
            panic!("controlled clock unwind before mutation");
        }
        if callback_mode == "storage" {
            std::fs::set_permissions(&callback_root, std::fs::Permissions::from_mode(0o777))
                .unwrap();
        }
    }));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owner.finalize_sync()));
    let status = supervisor.status();
    assert_eq!(status.busy, None);
    assert_eq!(clock.calls.load(SeqCst), before_calls + 1);
    assert_eq!(status.turns.len(), 1);
    if mode == "panic" {
        assert!(result.is_err());
        assert_eq!(
            budget.current_balance(&holder).await.unwrap(),
            CostTuple::ZERO
        );
        assert_eq!(
            status.turns[0].view.as_ref().unwrap().status,
            OwnedBudgetStatus::Active
        );
        assert!(status.turns[0].view.as_ref().unwrap().attempt.is_none());
        assert!(matches!(
            status.turns[0].error,
            Some(SettlementError::Panicked)
        ));
        assert!(matches!(owner.finalize_sync(), Err(SettlementError::State)));
        assert_eq!(clock.calls.load(SeqCst), before_calls + 1);
        drop(owner);
        assert_eq!(
            supervisor.status().turns[0].turn.rounds[0].decision,
            Some(SettlementDecision::Refusal(RefusalClass::Authorization))
        );
        assert!(supervisor.try_close().is_err());
    } else if mode == "storage" {
        assert!(matches!(
            result.unwrap(),
            Err(SettlementError::Storage(SettlementProblem::IdentityChanged))
        ));
        assert_eq!(
            budget.current_balance(&holder).await.unwrap(),
            CostTuple::cents(1)
        );
        let view = status.turns[0].view.as_ref().unwrap();
        assert_eq!(view.status, OwnedBudgetStatus::Finalized);
        assert_eq!(
            view.application.as_ref().unwrap().applied_debit,
            CostTuple::cents(9)
        );
        assert!(status.turns[0].undurable);
        let pending = (
            status.turns[0].pending_revision,
            status.turns[0].pending_digest,
        );
        assert!(pending.0.is_some());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let inventory = load_settlement_snapshot(&root, &receipt, &LIMITS).unwrap();
        assert!(matches!(
            inventory.snapshots[0].turn().rounds[0].phase,
            SettlementPhase::Prepared { .. }
        ));
        assert!(
            matches!(
                supervisor.resolve_storage(id),
                Err(SettlementError::Storage(SettlementProblem::IdentityChanged))
            ),
            "retry must preserve the store's actual quarantine instead of inventing recovery"
        );
        assert_eq!(
            (
                supervisor.status().turns[0].pending_revision,
                supervisor.status().turns[0].pending_digest
            ),
            pending
        );
        assert!(matches!(owner.finalize_sync(), Err(SettlementError::State)));
        assert_eq!(clock.calls.load(SeqCst), before_calls + 1);
        drop(owner);
        drop(coordinator);
        assert_eq!(
            supervisor.status().turns[0]
                .view
                .as_ref()
                .unwrap()
                .application
                .as_ref()
                .unwrap()
                .applied_debit,
            CostTuple::cents(9)
        );
        assert!(supervisor.try_close().is_err());
    } else {
        assert_eq!(mode, "healthy");
        result.unwrap().unwrap();
        owner.commit_disposition().unwrap();
        drop(owner);
        assert_eq!(
            budget.current_balance(&holder).await.unwrap(),
            CostTuple::cents(1)
        );
        assert!(supervisor.try_close().is_ok());
    }
    println!("PROBE_END:{mode}");
}
fn run_probe(
    mode: &str,
    deadline: std::time::Duration,
) -> (bool, std::process::ExitStatus, String) {
    use std::{
        io::Read,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.env_clear();
    for key in ["PATH", "HOME", "TMPDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let mut child = command
        .env("ARDUR_SETTLEMENT_PROBE_MODE", mode)
        .args([
            "--exact",
            "settlement_process_probe",
            "--nocapture",
            "--test-threads=1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            let mut buf = [0; 8192];
            loop {
                let n = pipe.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                let room = (128 * 1024usize).saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
            kept
        })
    }
    let out = drain(child.stdout.take().unwrap());
    let err = drain(child.stderr.take().unwrap());
    let start = Instant::now();
    let (timed_out, status) = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break (false, status);
        }
        if start.elapsed() >= deadline {
            child.kill().unwrap();
            break (true, child.wait().unwrap());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut bytes = out.join().unwrap();
    bytes.extend(err.join().unwrap());
    let text = String::from_utf8_lossy(&bytes).into_owned();
    println!("{text}");
    println!("PROBE_STATUS:{mode}:timed_out={timed_out}:status={status:?}");
    (timed_out, status, text)
}
fn probe_succeeded(mode: &str, result: &(bool, std::process::ExitStatus, String)) -> bool {
    !result.0
        && result.1.success()
        && result.2.contains(&format!("PROBE_BEGIN:{mode}"))
        && result.2.contains(&format!("PROBE_END:{mode}"))
}
#[test]
fn storage_failure_after_money_keeps_exact_pending_bytes_and_view() {
    let result = run_probe("storage", std::time::Duration::from_secs(15));
    assert!(
        probe_succeeded("storage", &result),
        "storage ownership probe failed: {result:?}"
    );
}
#[test]
fn reentry_unwind_and_callback_free_drop_are_process_supervised() {
    for mode in ["healthy", "panic", "drop"] {
        let result = run_probe(mode, std::time::Duration::from_secs(15));
        assert!(
            probe_succeeded(mode, &result),
            "ownership probe failed: {result:?}"
        );
    }
    let negative = run_probe("hang", std::time::Duration::from_secs(1));
    assert!(negative.0 && !negative.1.success());
    assert!(negative.2.contains("PROBE_BEGIN:hang"));
    assert!(
        !probe_succeeded("hang", &negative),
        "deadline/kill/reap cannot pass the success oracle"
    );
}
fn widest_snapshot(mut turn: TurnObligation, label: String) -> EncodedSnapshot {
    use ardur_session_journals::ReceiptId;
    let huge = all(u64::MAX);
    let share = all(1_000_000_000_000_000_000);
    let tool_sum = (0..LIMITS.max_tools_per_round)
        .try_fold(CostTuple::ZERO, |sum, _| sum.checked_add(&share))
        .unwrap();
    let jws = format!("a.{}.a", "a".repeat(LIMITS.max_receipt_bytes - 4));
    let candidate = ReceiptCandidate {
        receipt_id: ReceiptId(Uuid::new_v4()),
        expected_parent: Some(Sha256Digest::of(b"parent")),
        expected_log_end: u64::MAX,
        jws_digest: Sha256Digest::of(jws.as_bytes()),
        jws_compact: jws,
    };
    turn.verified_subject = HolderId(label.clone());
    turn.budget_holder = HolderId(label.clone());
    turn.journal_owner = Some(SessionId(Uuid::new_v4()));
    turn.started_at = UnixTsMillis(u64::MAX);
    turn.terminal = TurnTerminal::Unresolved(SettlementProblem::PriorEpochApplicationUnknown);
    turn.cancellation_marker = MarkerProjection::Prepared(candidate.clone());
    turn.rounds = (0..LIMITS.max_rounds)
        .map(|n| RoundObligation {
            settlement_id: SettlementId(Uuid::new_v4()),
            ordinal: n as u32,
            provider_request_id: Uuid::new_v4(),
            provider: label.clone(),
            model: label.clone(),
            request_digest: Sha256Digest::of(b"request"),
            reserved: huge,
            provider_evidence: ProviderEvidence::Observed {
                usage: Some(UsageSnapshot {
                    input_tokens: u64::MAX,
                    output_tokens: u64::MAX,
                }),
                cost: huge.checked_sub(&tool_sum).unwrap(),
                provenance: CostProvenance::PricedUsage(Sha256Digest::of(b"rates")),
                finished: false,
                interrupted: false,
            },
            tools: (0..LIMITS.max_tools_per_round)
                .map(|k| ToolEvidence {
                    ordinal: k as u32,
                    call_id: label.clone(),
                    name: label.clone(),
                    output_admission: OutputAdmission::NotScanned,
                    ..tool(share)
                })
                .collect(),
            known_incurred: huge,
            decision: Some(SettlementDecision::InfrastructureFailure(
                InfrastructureFailureClass::Provider,
            )),
            phase: SettlementPhase::Unresolved {
                last_definite: DefinitePhase::WorkObserved,
                problem: SettlementProblem::PriorEpochApplicationUnknown,
                application: Some(DebitApplication {
                    epoch: turn.budget_epoch,
                    requested_debit: huge,
                    applied_debit: huge,
                    reserved_credit: all(u64::MAX - 1),
                    additional_debit: all(u64::MAX - 1),
                    shortfall: CostTuple::ZERO,
                    rollback: RollbackStatus::Applied(huge),
                }),
                candidate: Some(candidate.clone()),
            },
            commit_ordinal: Some(u64::MAX),
            projection: JournalProjection::Acknowledged(Uuid::new_v4()),
        })
        .collect();
    EncodedSnapshot::new(u64::MAX, turn, &LIMITS).unwrap()
}
#[tokio::test]
async fn maximum_escaped_utf8_shapes_fit_reserved_record_headroom() {
    let f = Fixture::new(CostTuple::cents(10));
    let mut owner = f.owner(false).await;
    let base = f.saved()[0].turn().clone();
    // Quotes/backslashes are the maximal expansion among permitted metadata;
    // control characters are forbidden. Non-ASCII UTF-8 consumes its byte quota.
    let worst = widest_snapshot(base.clone(), "\"".repeat(LIMITS.max_metadata_bytes));
    let utf8 = widest_snapshot(
        base,
        format!("é{}", "\\".repeat(LIMITS.max_metadata_bytes - "é".len())),
    );
    assert!(
        f.coordinator.status().encoded_record_probe_bytes >= worst.bytes().len(),
        "constructor must actually validate full terminal/candidate/application headroom before work"
    );
    assert!(worst.bytes().len() <= LIMITS.max_record_bytes);
    assert!(utf8.bytes().len() <= LIMITS.max_record_bytes);
    assert_eq!(worst.turn().rounds.len(), 5);
    for round in &worst.turn().rounds {
        assert_eq!(round.tools.len(), 8);
    }
    println!(
        "MAX_SHAPE_BYTES={} UTF8_SHAPE_BYTES={} INVENTORY_WIRE_BYTES={}",
        worst.bytes().len(),
        utf8.bytes().len(),
        LIMITS.max_record_bytes * LIMITS.max_inventory_records
    );
    owner
        .observe_provider(observation(CostTuple::cents(9)))
        .unwrap();
    owner.abandon_sync().unwrap();
}
#[tokio::test]
async fn stop_after_slot_reservation_prevents_new_hold_without_fabricating_work() {
    let f = Fixture::new(CostTuple::cents(10));
    let owner = f
        .coordinator
        .reserve_turn(attribution(f.token, &f.holder, false))
        .unwrap();
    owner.close_empty().unwrap();
    assert_eq!(f.coordinator.status().inventory_count, 0);
    let mut owner = f
        .coordinator
        .reserve_turn(attribution(f.token, &f.holder, false))
        .unwrap();
    f.coordinator.stop_admission();
    assert!(
        matches!(
            owner.admit_round(request(f.token), Uuid::new_v4()).await,
            Err(SettlementError::AdmissionClosed)
        ),
        "a slot reservation is not permission to bypass a later admission stop"
    );
    assert_eq!(
        f.budget.current_balance(&f.holder).await.unwrap(),
        CostTuple::cents(10)
    );
    assert!(f.saved().is_empty());
    owner.close_empty().unwrap();
    assert!(f.coordinator.supervisor().try_close().is_ok());
}
#[tokio::test]
async fn single_execution_and_four_projection_slots_are_hard_limits() {
    let f = Fixture::new(CostTuple::cents(10));
    for expected in 1..=MAX_SLOTS {
        let owner = f.owner(true).await;
        assert!(matches!(
            f.coordinator
                .reserve_turn(attribution(f.token, &f.holder, true)),
            Err(SettlementError::Busy)
        ));
        drop(owner);
        assert_eq!(f.coordinator.status().turns.len(), expected);
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(10)
        );
    }
    assert!(matches!(
        f.coordinator
            .reserve_turn(attribution(f.token, &f.holder, true)),
        Err(SettlementError::SlotsFull)
    ));
    let status = f.coordinator.status();
    assert_eq!(status.inventory_count, MAX_SLOTS);
    let mut ordinals: Vec<_> = status
        .turns
        .iter()
        .map(|s| s.turn.rounds[0].commit_ordinal.unwrap())
        .collect();
    ordinals.sort_unstable();
    assert_eq!(ordinals, (0..MAX_SLOTS as u64).collect::<Vec<_>>());
    assert!(
        f.coordinator.supervisor().try_close().is_err(),
        "durable pending projections still own backlog slots"
    );
}
#[tokio::test]
async fn sixty_four_retained_snapshots_block_new_turn_but_last_turn_advances() {
    let f = Fixture::new(CostTuple::cents(10));
    for _ in 0..LIMITS.max_inventory_records - 1 {
        drop(f.owner(false).await);
    }
    assert_eq!(f.coordinator.status().inventory_count, 63);
    let mut last = f.owner(false).await;
    assert_eq!(f.coordinator.status().inventory_count, 64);
    last.observe_provider(ProviderEvidence::DispatchIntent)
        .unwrap();
    last.observe_provider(observation(CostTuple::cents(9)))
        .unwrap();
    last.abandon_sync().unwrap();
    drop(last);
    assert_eq!(f.saved().len(), 64);
    assert!(f.coordinator.status().turns.is_empty());
    assert!(matches!(
        f.coordinator
            .reserve_turn(attribution(f.token, &f.holder, false)),
        Err(SettlementError::InventoryFull)
    ));
    assert_eq!(
        f.budget.current_balance(&f.holder).await.unwrap(),
        CostTuple::cents(10)
    );
    drop(f.coordinator);
    let reopened = SettlementCoordinator::open(f.gate.clone(), &f.root, f.receipt.clone()).unwrap();
    assert_eq!(reopened.status().inventory_count, 64);
    assert!(matches!(
        reopened.reserve_turn(attribution(f.token, &f.holder, false)),
        Err(SettlementError::InventoryFull)
    ));
}
#[tokio::test]
async fn prior_epoch_application_blocks_without_replaying_new_budget() {
    let f = Fixture::new(CostTuple::cents(10));
    let mut owner = f.owner(false).await;
    owner
        .observe_provider(observation(CostTuple::cents(9)))
        .unwrap();
    owner
        .prepare(
            Disposition::Infrastructure(InfrastructureFailureClass::Provider),
            CostTuple::cents(9),
        )
        .unwrap();
    owner.finalize_sync().unwrap();
    let old_epoch = f.coordinator.status().epoch;
    drop(owner);
    drop(f.coordinator);
    let fresh = SharedBudget::new();
    fresh.set_balance(f.holder.clone(), all(100));
    let gate = Arc::new(InMemoryCostAdmissionGate::new(fresh.clone()));
    gate.bind_token(f.token, f.holder.clone());
    let reopened = SettlementCoordinator::open(gate, &f.root, f.receipt.clone()).unwrap();
    assert_ne!(reopened.status().epoch, old_epoch);
    assert_eq!(
        reopened.status().boot_problem,
        Some(SettlementProblem::PriorEpochApplicationUnknown)
    );
    assert!(matches!(
        reopened.reserve_turn(attribution(f.token, &f.holder, false)),
        Err(SettlementError::AdmissionClosed)
    ));
    assert_eq!(fresh.current_balance(&f.holder).await.unwrap(), all(100));
}
#[tokio::test]
async fn oversized_tool_retains_original_allocation_and_refuses_further_effects() {
    let f = Fixture::new(CostTuple::cents(10));
    let mut owner = f.owner(false).await;
    owner
        .observe_provider(observation(CostTuple::cents(9)))
        .unwrap();
    let mut oversized = tool(all(1));
    oversized.name = "é".repeat(32_769);
    let pointer = oversized.name.as_ptr();
    let original_len = oversized.name.len();
    assert!(matches!(
        owner.observe_tool(oversized),
        Err(SettlementError::Invalid(SettlementProblem::Bounds))
    ));
    let status = f.coordinator.status();
    let RejectedObservation::Tool(raw) = status.turns[0].rejected.as_deref().unwrap() else {
        panic!("exact raw tool retained")
    };
    assert_eq!(
        raw.name.as_ptr(),
        pointer,
        "move the already allocated component, do not clone oversized metadata"
    );
    assert_eq!(raw.name.len(), original_len);
    assert_eq!(raw.effect, tool(all(1)).effect);
    assert!(status.turns[0].undurable);
    assert!(matches!(
        owner.observe_tool(tool(all(0))),
        Err(SettlementError::AdmissionClosed)
    ));
    assert_eq!(
        f.saved()[0].turn().rounds[0].known_incurred,
        CostTuple::cents(9)
    );
    assert!(f.saved()[0].turn().rounds[0].tools.is_empty());
    let supervisor = f.coordinator.supervisor();
    drop(owner);
    assert!(supervisor.try_close().is_err());
}
#[tokio::test]
async fn provider_regressions_retain_exact_rejected_observation() {
    for dimension in 0..9 {
        let f = Fixture::new(CostTuple::cents(10));
        let mut owner = f.owner(false).await;
        let mut first = observation(all(9));
        if let ProviderEvidence::Observed {
            finished,
            interrupted,
            ..
        } = &mut first
        {
            *finished = true;
            *interrupted = true;
        }
        owner
            .observe_provider(ProviderEvidence::DispatchIntent)
            .unwrap();
        owner.observe_provider(first.clone()).unwrap();
        let mut regressed = first.clone();
        if let ProviderEvidence::Observed {
            cost,
            usage,
            provenance,
            finished,
            interrupted,
        } = &mut regressed
        {
            match dimension {
                0 => cost.tokens_in = 8,
                1 => cost.tokens_out = 8,
                2 => cost.cents = 8,
                3 => cost.wall_ms = 8,
                4 => cost.attention_score = 8,
                5 => *usage = None,
                6 => *provenance = CostProvenance::ResponseCost,
                7 => *finished = false,
                8 => *interrupted = false,
                _ => unreachable!(),
            }
        }
        assert!(
            matches!(
                owner.observe_provider(regressed.clone()),
                Err(SettlementError::Storage(
                    SettlementProblem::InvalidTransition
                ))
            ),
            "dimension {dimension}"
        );
        assert_eq!(
            f.coordinator.status().turns[0].rejected.as_deref(),
            Some(&RejectedObservation::Provider(regressed))
        );
        assert_eq!(f.saved()[0].turn().rounds[0].provider_evidence, first);
        assert!(matches!(
            owner.abandon_sync(),
            Err(SettlementError::AdmissionClosed)
        ));
    }
}
#[tokio::test]
async fn intent_cannot_be_erased_and_unknown_tool_needs_conservative_refinement() {
    let f = Fixture::new(CostTuple::cents(10));
    let mut owner = f.owner(false).await;
    owner
        .observe_provider(ProviderEvidence::DispatchIntent)
        .unwrap();
    assert!(matches!(
        owner.observe_provider(ProviderEvidence::NotDispatched),
        Err(SettlementError::Storage(
            SettlementProblem::InvalidTransition
        ))
    ));
    assert_eq!(
        f.saved()[0].turn().rounds[0].provider_evidence,
        ProviderEvidence::DispatchIntent
    );
    let other = Fixture::new(CostTuple::cents(10));
    let mut owner = other.owner(false).await;
    let mut unknown = tool(CostTuple::ZERO);
    unknown.effect = ToolEffect::InterruptedUnknown;
    unknown.output_admission = OutputAdmission::NotScanned;
    owner.observe_tool(unknown.clone()).unwrap();
    unknown.effect = ToolEffect::NotInvoked(RefusalClass::Authorization);
    assert!(matches!(
        owner.observe_tool(unknown),
        Err(SettlementError::Storage(
            SettlementProblem::InvalidTransition
        ))
    ));
    assert_eq!(
        other.saved()[0].turn().rounds[0].tools[0].effect,
        ToolEffect::InterruptedUnknown
    );
}
#[tokio::test]
async fn verified_late_tool_result_and_provider_cost_aggregate_before_cancel() {
    let f = Fixture::new(CostTuple::cents(10));
    let mut owner = f.owner(false).await;
    let mut unknown = tool(CostTuple::ZERO);
    unknown.effect = ToolEffect::InterruptedUnknown;
    unknown.output_admission = OutputAdmission::NotScanned;
    owner.observe_tool(unknown).unwrap();
    owner.observe_tool(tool(all(3))).unwrap();
    owner.observe_provider(observation(all(2))).unwrap();
    assert_eq!(f.saved()[0].turn().rounds[0].known_incurred, all(5));
    owner.abandon_sync().unwrap();
    owner.abandon_sync().unwrap();
    drop(owner);
    assert_eq!(
        f.budget.current_balance(&f.holder).await.unwrap(),
        CostTuple::cents(10)
    );
    assert_eq!(f.saved()[0].turn().rounds[0].known_incurred, all(5));
}
#[tokio::test]
async fn no_work_cancel_and_later_drop_never_refund_an_earlier_committed_charge() {
    let f = Fixture::new(CostTuple::cents(30));
    let mut paid = f.owner(false).await;
    paid.observe_provider(observation(CostTuple::cents(9)))
        .unwrap();
    paid.prepare(
        Disposition::Refusal(RefusalClass::Authorization),
        CostTuple::cents(9),
    )
    .unwrap();
    paid.finalize_sync().unwrap();
    paid.commit_disposition().unwrap();
    let first = f.saved()[0].clone();
    drop(paid);
    let mut uncommitted = f.owner(false).await;
    uncommitted.abandon_sync().unwrap();
    uncommitted.abandon_sync().unwrap();
    drop(uncommitted);
    assert_eq!(
        f.budget.current_balance(&f.holder).await.unwrap(),
        CostTuple::cents(21)
    );
    let records = f.saved();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records
            .iter()
            .find(|s| s.turn().turn_id == first.turn().turn_id)
            .unwrap()
            .bytes(),
        first.bytes()
    );
    let no_work = records
        .iter()
        .find(|s| s.turn().turn_id != first.turn().turn_id)
        .unwrap();
    assert_eq!(
        no_work.turn().rounds[0].provider_evidence,
        ProviderEvidence::NotDispatched
    );
    assert_eq!(no_work.turn().rounds[0].known_incurred, CostTuple::ZERO);
}
#[tokio::test]
async fn gate_error_retains_attempt_and_claimed_authority() {
    let f = Fixture::new(CostTuple::cents(10));
    let mut owner = f.owner(false).await;
    owner.observe_provider(observation(all(u64::MAX))).unwrap();
    owner
        .prepare(
            Disposition::Infrastructure(InfrastructureFailureClass::Budget),
            all(u64::MAX),
        )
        .unwrap();
    let error = owner.finalize_sync().unwrap_err();
    let SettlementError::Budget(error) = error else {
        panic!("real typed gate failure")
    };
    let AdmissionError::Internal(error) = error.as_ref() else {
        panic!("real range failure")
    };
    assert!(error.downcast_ref::<OwnedDebitUnrepresentable>().is_some());
    let status = f.coordinator.status();
    let view = status.turns[0].view.as_ref().unwrap();
    assert_eq!(view.attempt.unwrap().requested_debit, all(u64::MAX));
    assert_eq!(view.attempt.unwrap().known_incurred, all(u64::MAX));
    assert!(view.application.is_none());
    assert_eq!(view.status, OwnedBudgetStatus::Active);
    assert_eq!(
        f.budget.current_balance(&f.holder).await.unwrap(),
        CostTuple::ZERO
    );
    let supervisor = f.coordinator.supervisor();
    drop(owner);
    assert_eq!(
        supervisor.status().turns[0].turn.rounds[0].decision,
        Some(SettlementDecision::InfrastructureFailure(
            InfrastructureFailureClass::Budget
        ))
    );
    assert!(supervisor.try_close().is_err());
}
#[tokio::test]
async fn unavailable_storage_keeps_newer_observation_and_cancel_facts() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(CostTuple::cents(10));
    let mut owner = f.owner(false).await;
    owner
        .observe_provider(observation(CostTuple::cents(9)))
        .unwrap();
    let id = owner.turn_id();
    let supervisor = f.coordinator.supervisor();
    std::fs::set_permissions(&f.root, std::fs::Permissions::from_mode(0o777)).unwrap();
    let newer = observation(CostTuple::cents(11));
    assert!(matches!(
        owner.observe_provider(newer.clone()),
        Err(SettlementError::Storage(SettlementProblem::IdentityChanged))
    ));
    let pending = supervisor.status().turns[0].pending_digest;
    drop(owner);
    drop(f.coordinator);
    assert_eq!(
        f.budget.current_balance(&f.holder).await.unwrap(),
        CostTuple::cents(10)
    );
    let status = supervisor.status();
    assert_eq!(status.turns[0].pending_digest, pending);
    assert_eq!(
        status.turns[0].rejected.as_deref(),
        Some(&RejectedObservation::Provider(newer))
    );
    assert_eq!(
        status.turns[0].view.as_ref().unwrap().status,
        OwnedBudgetStatus::Released
    );
    assert_eq!(status.turns[0].turn.terminal, TurnTerminal::Cancelled);
    assert_eq!(
        status.turns[0].turn.rounds[0].known_incurred,
        CostTuple::cents(11)
    );
    std::fs::set_permissions(&f.root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let saved = load_settlement_snapshot(&f.root, &f.receipt, &LIMITS).unwrap();
    assert_eq!(
        saved.snapshots[0].turn().rounds[0].known_incurred,
        CostTuple::cents(9),
        "newer rejected snapshot is not claimed durable"
    );
    assert!(matches!(
        supervisor.resolve_storage(id),
        Err(SettlementError::Storage(SettlementProblem::IdentityChanged))
    ));
    assert!(supervisor.try_close().is_err());
}
#[tokio::test]
async fn storage_quarantine_is_not_overwritten_by_pending_credit() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(CostTuple::cents(10));
    let mut owner = f.owner(false).await;
    owner
        .observe_provider(observation(CostTuple::cents(9)))
        .unwrap();
    f.budget
        .provision_merge(&f.holder, &CostTuple::cents(u64::MAX - 3), None)
        .await
        .unwrap();
    std::fs::set_permissions(&f.root, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(
        owner.abandon_sync(),
        Err(SettlementError::Storage(SettlementProblem::IdentityChanged))
    ));
    let status = f.coordinator.status();
    assert!(
        matches!(
            status.turns[0].error,
            Some(SettlementError::Storage(SettlementProblem::IdentityChanged))
        ),
        "status must retain the actual storage failure as well as partial money facts"
    );
    let view = status.turns[0].view.as_ref().unwrap();
    assert_eq!(view.status, OwnedBudgetStatus::ReleasePending);
    assert_eq!(
        view.refund.as_ref().unwrap().applied_credit,
        CostTuple::cents(3)
    );
    assert_eq!(
        view.refund.as_ref().unwrap().remaining_credit,
        CostTuple::cents(7)
    );
    drop(owner);
    std::fs::set_permissions(&f.root, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(f.coordinator.supervisor().try_close().is_err());
}
#[tokio::test]
async fn paid_refusal_records_actual_movement_before_commit() {
    let f = Fixture::new(CostTuple::cents(10));
    let mut owner = f.owner(false).await;
    owner
        .observe_provider(observation(CostTuple::cents(9)))
        .unwrap();
    assert!(
        owner
            .prepare(
                Disposition::Refusal(RefusalClass::Authorization),
                CostTuple::cents(9)
            )
            .is_ok(),
        "supported refusal must freeze durably before money"
    );
    assert_eq!(
        f.budget.current_balance(&f.holder).await.unwrap(),
        CostTuple::ZERO
    );
    assert!(matches!(
        f.saved()[0].turn().rounds[0].phase,
        SettlementPhase::Prepared { candidate: None }
    ));
    owner.finalize_sync().unwrap();
    let saved = f.saved();
    let SettlementPhase::Finalized {
        application,
        candidate: None,
    } = &saved[0].turn().rounds[0].phase
    else {
        panic!("actual finalized movement must be durable")
    };
    assert_eq!(application.applied_debit, CostTuple::cents(9));
    assert_eq!(application.reserved_credit, CostTuple::cents(1));
    assert_eq!(application.rollback, RollbackStatus::None);
    assert_eq!(
        f.budget.current_balance(&f.holder).await.unwrap(),
        CostTuple::cents(1)
    );
    assert!(
        matches!(owner.finalize_sync(), Err(SettlementError::State)),
        "never finalize twice"
    );
    owner.commit_disposition().unwrap();
    drop(owner);
    assert_eq!(
        f.budget.current_balance(&f.holder).await.unwrap(),
        CostTuple::cents(1),
        "Drop must not compensate a paid refusal"
    );
    let saved = f.saved();
    assert!(matches!(
        saved[0].turn().rounds[0].phase,
        SettlementPhase::Settled { receipt: None, .. }
    ));
    assert_eq!(
        saved[0].turn().terminal,
        TurnTerminal::Refused(RefusalClass::Authorization)
    );
}
#[tokio::test]
async fn admitted_known_cost_cancel_is_durable_without_caller_debit() {
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().canonicalize().unwrap();
    let root = parent.join("settlement");
    let receipt = identity(&parent);
    let budget = SharedBudget::new();
    let holder = HolderId("actual-budget-holder".into());
    budget.set_balance(holder.clone(), CostTuple::cents(10));
    let gate = Arc::new(InMemoryCostAdmissionGate::new(budget.clone()));
    let token = TokenId(Uuid::new_v4());
    gate.bind_token(token, holder.clone());
    let coordinator = SettlementCoordinator::open(gate, &root, receipt.clone()).unwrap();
    let who = attribution(token, &holder, true);
    let reserved = coordinator.reserve_turn(who.clone());
    assert!(
        reserved.is_ok(),
        "a healthy store must reserve real turn capacity"
    );
    let mut owner = reserved.unwrap();
    owner
        .admit_round(request(token), Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(
        budget.current_balance(&holder).await.unwrap(),
        CostTuple::ZERO
    );
    let initial = load_settlement_snapshot(&root, &receipt, &LIMITS).unwrap();
    assert_eq!(
        initial.snapshots[0].turn().rounds[0].provider_evidence,
        ProviderEvidence::NotDispatched
    );
    owner
        .observe_provider(ProviderEvidence::DispatchIntent)
        .unwrap();
    owner
        .observe_provider(observation(CostTuple::cents(9)))
        .unwrap();
    owner.abandon_sync().unwrap();
    assert_eq!(
        budget.current_balance(&holder).await.unwrap(),
        CostTuple::cents(10)
    );
    let inventory = load_settlement_snapshot(&root, &receipt, &LIMITS).unwrap();
    assert_eq!(inventory.snapshots.len(), 1);
    let turn = inventory.snapshots[0].turn();
    assert_eq!(turn.request_session, who.request_session);
    assert_eq!(turn.journal_owner, who.journal_owner);
    assert_ne!(Some(turn.request_session), turn.journal_owner);
    assert_ne!(turn.verified_subject, turn.budget_holder);
    assert_eq!(turn.budget_holder, holder);
    assert_eq!(turn.cap_token_id, token);
    assert_eq!(turn.terminal, TurnTerminal::Cancelled);
    let round = &turn.rounds[0];
    assert_eq!(round.known_incurred, CostTuple::cents(9));
    assert_eq!(round.decision, Some(SettlementDecision::Cancelled));
    let SettlementPhase::Settled {
        application,
        receipt,
    } = &round.phase
    else {
        panic!("actual release must be settled")
    };
    assert_eq!(application.requested_debit, CostTuple::ZERO);
    assert_eq!(application.reserved_credit, CostTuple::cents(10));
    assert_eq!(application.applied_debit, CostTuple::ZERO);
    assert!(receipt.is_none(), "no completed receipt is fabricated");
}
