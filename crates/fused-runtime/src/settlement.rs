//! Bounded settlement ownership for trusted in-process embedders.
//!
//! Runtime dispatch binds authenticated identities and observations to this
//! owner before provisioning/admission. It admits through the concrete gate;
//! callers cannot substitute token metadata on a cloned reservation. Budgets
//! remain process-local; serialized applications are evidence, never replay authority.
//!
//! Supply a durably established trusted parent, stable namespace and no ACL
//! grants to other principals (the store checks private Unix modes/ownership).
//! Synchronous filesystem I/O has no finite latency guarantee. No background
//! task, executor, TTL eviction, history pruning or persistent budget is added.
//!
//! The concrete admission call is used to bind the original request token, not
//! mutable reservation metadata. Its pre-claim behavior remains the gate's
//! contract: in particular a panic in generic admission's clock after reservation
//! but before returning its handle is not made lossless by this adapter. Once a
//! Reservation is returned, it is retained even if the subsequent claim unwinds.
//! Claimed operations use stack handback without any coordinator/store lock.
//! Attempt inputs are retained before entering admission. A gate error or panic
//! without returned authority stays conservatively quarantined; no new reclaim
//! policy is invented. `close_empty` accepts only proven-unused capacity. Storage
//! acknowledgement never clears holder validation or pending credit. Owner Drop
//! records abandonment even while Busy; handback drains a bounded pass outside
//! locks, with explicit supervisor retry if an unwind/contention prevents it.
//!
//! Explicit supervisor drains project settled economics to the configured
//! journal. Generic ambiguous acknowledgements remain quarantined with exact
//! attempts retained; restart never reapplies budgets. Receipt success and up
//! to five rounds share this owner. The 64-record wire ceiling is 16 MiB, not
//! a measured memory ceiling or physical disk preallocation. Invalid allocated
//! input is retained when rejected, not erased. No transcript is retained here.
use crate::SharedBudget;
use ardur_cost_gate::{
    AdmissionError, AdmissionRequest, CostAdmissionGate, CostTuple, HolderId,
    InMemoryCostAdmissionGate, OwnedBudgetStatus, OwnedBudgetView, Reservation,
    SettlementReservation, TokenId, UnixTsMillis,
};
use ardur_session_journals::{SessionId, settlement::*};
use parking_lot::Mutex;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};
use uuid::Uuid;

/// Practical encoded evidence limits, not a measured RSS guarantee.
pub const LIMITS: SettlementLimits = SettlementLimits {
    max_record_bytes: 262_144,
    max_metadata_bytes: 512,
    max_rounds: 5,
    max_tools_per_round: 8,
    max_receipt_bytes: 16_384,
    max_inventory_records: 64,
};
/// Maximum executing plus retained-backlog turns. No slot is evicted.
pub const MAX_SLOTS: usize = 4;
/// Attribution supplied by a trusted, already-authenticated embedder.
#[derive(Clone, Debug)]
pub struct TurnIdentity {
    /// Request attribution, independent of the configured journal.
    pub request_session: SessionId,
    /// Actual configured journal owner.
    pub journal_owner: Option<SessionId>,
    /// Verified subject, not necessarily the budget holder.
    pub verified_subject: HolderId,
    /// Trusted provisioned holder, checked against the live capability.
    pub budget_holder: HolderId,
    /// Token identifier only, never a bearer token.
    pub cap_token_id: TokenId,
    /// Stable caller-sampled timestamp; no clock is invoked by cancellation.
    pub started_at: UnixTsMillis,
}
/// Supported non-receipt outcome selection. Runtime receipt methods are crate-private.
#[derive(Clone, Copy, Debug)]
pub enum Disposition {
    /// A non-cancellation refusal.
    Refusal(RefusalClass),
    /// A definite infrastructure failure.
    Infrastructure(InfrastructureFailureClass),
}
/// Typed owner failures. An error never transfers or destroys claimed authority.
#[derive(Clone, Debug, thiserror::Error)]
pub enum SettlementError {
    /// Operation incompatible with current ownership state.
    #[error("incompatible settlement state")]
    State,
    /// Another synchronous operation owns the handback guard.
    #[error("settlement operation busy")]
    Busy,
    /// Admission stopped or quarantined; inspect the retained status.
    #[error("settlement admission closed")]
    AdmissionClosed,
    /// All live/backlog slots are occupied.
    #[error("settlement slots full")]
    SlotsFull,
    /// Retained history plus reserved membership reached the inventory limit.
    #[error("settlement inventory full")]
    InventoryFull,
    /// Trusted attribution disagreed with the actual claimed holder/token.
    #[error("settlement identity mismatch")]
    IdentityMismatch,
    /// A rejected encoding is not durable evidence.
    #[error("invalid settlement evidence: {0:?}")]
    Invalid(SettlementProblem),
    /// Retained exact bytes have not received a durable acknowledgement.
    #[error("settlement storage unresolved: {0:?}")]
    Storage(SettlementProblem),
    /// Actual credit remains unpaid. No nominal refund establishes closure.
    #[error("settlement credit pending: {0:?}")]
    CreditPending(SettlementProblem),
    /// A concrete store could not be opened or inventoried.
    #[error("settlement store open failed")]
    Open(#[source] Arc<SettlementStoreError>),
    /// A borrowed gate operation failed; its latest view is retained separately.
    #[error("settlement budget operation failed")]
    Budget(#[source] Arc<AdmissionError>),
    /// Exact receipt candidate may have been appended; manual resolution required.
    #[error("settlement receipt append unresolved")]
    ReceiptUnknown,
    /// A secondary journal append may have happened; no blind retry or refund.
    #[error("settlement journal projection unresolved")]
    ProjectionUnknown,
    /// A journal explicitly proved it did not write the selected record.
    #[error("settlement journal projection definitely not applied")]
    ProjectionNotApplied,
    /// An operation unwound; live authority and the latest gate view remain owned.
    #[error("settlement operation panicked")]
    Panicked,
}

/// Original rejected component. No arguments, output text or bearer credentials.
/// Moving an already allocated oversized input here is not a hard RSS bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RejectedObservation {
    /// Cumulative provider observation.
    Provider(ProviderEvidence),
    /// One tool observation, including its exact cost/digests/labels.
    Tool(ToolEvidence),
}
/// Frozen economic admission inputs, retained even if admission or claiming fails.
#[derive(Clone, Debug)]
pub struct AdmissionAttempt {
    /// Original trusted request, including token identifier and projected hold.
    pub request: AdmissionRequest,
    /// Original provider request identity; never replaced on retry.
    pub provider_request_id: Uuid,
}
/// Read-only facts, not authority to mutate a budget or authenticate a receipt.
#[derive(Clone, Debug)]
pub struct TurnSettlementStatus {
    /// Latest local typed observations (not necessarily acknowledged).
    pub turn: TurnObligation,
    /// Most recent real gate view; while busy, the pre-operation view.
    pub view: Option<OwnedBudgetView>,
    /// Retained typed reason, if quarantined.
    pub error: Option<SettlementError>,
    /// Last durably acknowledged revision.
    pub acknowledged_revision: Option<u64>,
    /// Immutable pending revision; never regenerated for retry.
    pub pending_revision: Option<u64>,
    /// Exact pending-byte digest.
    pub pending_digest: Option<ardur_cost_gate::Sha256Digest>,
    /// Local facts exceed the last acknowledged snapshot.
    pub undurable: bool,
    /// Admitted hold retained if claiming unwound before transfer.
    pub unclaimed_reservation: Option<Uuid>,
    /// Original attempted inputs, including uncertain attempts with no handle.
    pub admission_attempt: Option<AdmissionAttempt>,
    /// Original component that could not be durably accepted.
    pub rejected: Option<Arc<RejectedObservation>>,
    /// Owner disappeared before cancellation could select a decision.
    pub abandonment_pending: bool,
    /// A selected durable disposition still needs its existing capability retired.
    pub retirement_pending: bool,
    /// Frozen trusted expectation, separate from the actual holder in `turn`.
    pub expected_holder: HolderId,
    /// Validation failure retained independently of primary I/O/credit errors.
    pub validation_error: Option<SettlementError>,
    /// Exact in-flight/ambiguous secondary attempts; never blindly retried.
    pub pending_projections: Vec<(Uuid, ardur_session_journals::JournalEntry)>,
}
/// Bounded coordinator status. Metadata contains identifiers, not bearer tokens.
#[derive(Clone, Debug)]
pub struct SettlementStatus {
    /// Fresh process budget epoch.
    pub epoch: BudgetEpoch,
    /// Turn still held by its executing owner handle.
    pub executing: Option<TurnId>,
    /// Synchronous operation holding the stack handback guard.
    pub busy: Option<TurnId>,
    /// Live/backlog slots, including a busy slot's pre-operation facts.
    pub turns: Vec<TurnSettlementStatus>,
    /// Retained history plus reserved not-yet-written membership.
    pub inventory_count: usize,
    /// Actual maximum-shape encoding checked before opening/admitting work.
    pub encoded_record_probe_bytes: usize,
    /// Explicit stop, independent of economic/storage quarantine.
    pub stopped: bool,
    /// Unsupported prior-process recovery closes admission without replay.
    pub boot_problem: Option<SettlementProblem>,
}
/// External retaining handle, not a worker. Keep at least one supervisor until
/// safe closure. Dropping all owners with nondurable facts is outside this
/// supervised model; no arbitrary-death losslessness is claimed.
#[derive(Clone)]
pub struct SettlementSupervisor(Arc<SettlementCoordinator>);
impl SettlementSupervisor {
    /// Retry retained owner disappearance after a busy/unwinding operation.
    /// Unselected cancellation keeps admission closed; no decision is relabelled.
    pub fn retry_abandonment(&self, turn: TurnId) -> Result<(), SettlementError> {
        self.0.drain_abandoned();
        let state = self.0.state.lock();
        if state.abandoned.contains(&turn) {
            Err(SettlementError::AdmissionClosed)
        } else {
            state
                .slots
                .get(&turn)
                .and_then(Option::as_ref)
                .map_or(Ok(()), Slot::outcome)
        }
    }
    /// Read authoritative snapshots from the leased store namespace.
    pub fn durable_snapshots(&self) -> Result<Vec<EncodedSnapshot>, SettlementError> {
        load_settlement_snapshot(&self.0.root, &self.0.identity, &LIMITS)
            .map(|i| i.snapshots)
            .map_err(|e| SettlementError::Open(Arc::new(e)))
    }
    /// Inspect retained evidence without exposing a capability.
    pub fn status(&self) -> SettlementStatus {
        self.0.status()
    }
    /// Stop new turns, preserving all pending work.
    pub fn stop_admission(&self) {
        self.0.stop_admission();
    }
    /// Retry only a live release/rollback remainder after its pending facts are durable.
    pub fn retry_refund(&self, turn: TurnId) -> Result<(), SettlementError> {
        let mut handback = self.0.take(turn)?;
        let slot = handback.slot.as_mut().expect("owned slot");
        if slot.pending.is_some() || slot.dirty {
            return Err(SettlementError::AdmissionClosed);
        }
        let view = slot.view.as_ref().ok_or(SettlementError::State)?;
        let rollback = match view.status {
            OwnedBudgetStatus::ReleasePending => false,
            OwnedBudgetStatus::RollbackPending => true,
            _ => return Err(SettlementError::State),
        };
        let known = view.attempt.ok_or(SettlementError::State)?.known_incurred;
        let result = if rollback {
            self.0
                .gate
                .rollback_owned_sync(slot.token.as_mut().ok_or(SettlementError::State)?)
        } else {
            self.0
                .gate
                .release_owned_sync(slot.token.as_mut().ok_or(SettlementError::State)?, known)
        };
        slot.refresh(&self.0)?;
        if rollback {
            slot.record_application(self.0.epoch);
        } else {
            slot.record_release(self.0.epoch);
        }
        if let Err(e) = result {
            let error = SettlementError::Budget(Arc::new(e));
            slot.error = Some(error.clone());
            return Err(error);
        }
        slot.persist(&self.0)?;
        slot.outcome()
    }
    /// Resolve/retry only the immutable pending bytes before encoding newer facts.
    /// No debit/refund is repeated. An acknowledged Settled disposition also
    /// retires its existing Finalized capability. Permission restoration need not heal a
    /// store quarantined before it installed a recoverable pending attempt.
    pub fn resolve_storage(&self, turn: TurnId) -> Result<(), SettlementError> {
        let mut handback = self.0.take(turn)?;
        let slot = handback.slot.as_mut().expect("owned slot");
        if slot.pending.is_none() && slot.retirement_pending {
            return slot.retire_acknowledged(&self.0);
        }
        let pending = slot.pending.as_ref().ok_or(SettlementError::State)?;
        let mut store = self.0.store.lock();
        let mut resolution =
            store.resolve_exact(turn, pending.encoded.revision(), pending.encoded.digest());
        if resolution == WriteResolution::DefinitelyNotApplied {
            // Still exactly the same immutable bytes/predecessor. Absence does
            // not by itself clear the store's quarantine.
            resolution = store.put_exact(pending.previous, &pending.encoded);
        }
        drop(store);
        slot.acknowledge(resolution)?;
        if slot.dirty {
            slot.persist(&self.0)?;
        }
        slot.rejected = None;
        slot.retire_acknowledged(&self.0)?;
        // Storage health alone does not discharge a credit obligation.
        if let Some(round) = slot.turn.rounds.last() {
            if let SettlementPhase::Unresolved {
                problem:
                    problem @ (SettlementProblem::ReleaseCreditPending
                    | SettlementProblem::RollbackCreditPending),
                ..
            } = round.phase
            {
                slot.error = Some(SettlementError::CreditPending(problem));
            }
        }
        slot.outcome()
    }
    /// Explicit full compensation for a trusted definite failure before closure.
    /// The embedder selects this outcome under its existing financial policy;
    /// it is never an automatic way to erase a paid refusal or an excess credit.
    pub fn compensate_definite_failure(&self, turn: TurnId) -> Result<(), SettlementError> {
        let mut handback = self.0.take(turn)?;
        let slot = handback.slot.as_mut().expect("owned slot");
        if slot.pending.is_some() || slot.dirty {
            return Err(SettlementError::AdmissionClosed);
        }
        if slot
            .view
            .as_ref()
            .is_none_or(|v| v.status != OwnedBudgetStatus::Finalized)
            || matches!(
                slot.turn.rounds.last_mut().expect("current round").phase,
                SettlementPhase::Settled { .. }
            )
        {
            return Err(SettlementError::State);
        }
        let result = self
            .0
            .gate
            .rollback_owned_sync(slot.token.as_mut().ok_or(SettlementError::State)?);
        slot.refresh(&self.0)?;
        slot.record_application(self.0.epoch);
        if let Err(e) = result {
            let error = SettlementError::Budget(Arc::new(e));
            slot.error = Some(error.clone());
            return Err(error);
        }
        slot.persist(&self.0)?;
        slot.outcome()
    }
    /// Project a bounded pass of durably selected settlements. No lock crosses
    /// an await. Dropped/ambiguous attempts remain latched, never re-appended.
    /// Budgets are never replayed, including in a fresh process epoch.
    pub async fn drain_pending(
        &self,
        journal: &dyn ardur_session_journals::SessionJournal,
    ) -> Result<usize, SettlementError> {
        self.0.drain_abandoned();
        let mut work = Vec::new();
        for status in self.status().turns {
            for round in &status.turn.rounds {
                if let JournalProjection::Pending(id) = round.projection {
                    work.push((round.commit_ordinal, status.turn.turn_id, round.ordinal, id));
                }
            }
        }
        work.sort_by_key(|item| item.0);
        let mut count = 0;
        for (_, turn, ordinal, id) in work {
            let entry = {
                let mut handback = self.0.take(turn)?;
                let slot = handback.slot.as_mut().expect("owned slot");
                slot.outcome()?;
                if slot.pending.is_some() || slot.dirty {
                    return Err(SettlementError::AdmissionClosed);
                }
                if slot.turn.journal_owner != Some(*journal.session_id()) {
                    return Err(SettlementError::IdentityMismatch);
                }
                if slot.projection_attempts.contains_key(&id) {
                    return Err(SettlementError::ProjectionUnknown);
                }
                let round = &slot.turn.rounds[ordinal as usize];
                let application = match &round.phase {
                    SettlementPhase::Settled { application, .. } => application,
                    SettlementPhase::Finalized { application, .. }
                        if !matches!(
                            round.decision,
                            Some(SettlementDecision::Completion { .. })
                        ) =>
                    {
                        application
                    }
                    _ => continue,
                };
                let reservation_id =
                    ardur_session_journals::ReservationId::from_uuid(round.settlement_id.0);
                let compensated = matches!(application.rollback, RollbackStatus::Applied(credit) if credit == application.applied_debit);
                let entry = if round.known_incurred != CostTuple::ZERO
                    && (round.decision == Some(SettlementDecision::Cancelled)
                        || compensated
                        || application.requested_debit == CostTuple::ZERO)
                {
                    ardur_session_journals::JournalEntry::OperatorExpense {
                        session_id: slot.turn.request_session, reservation_id,
                        provider_cost: round.known_incurred, class: if compensated || matches!(round.decision, Some(SettlementDecision::InfrastructureFailure(_))) { "compensated_failure" } else { "cancelled_precommit" }.into(),
                        reason: if compensated || matches!(round.decision, Some(SettlementDecision::InfrastructureFailure(_))) { "settlement compensation; known operator expense retained" } else { "settlement cancellation; caller refunded, known operator expense retained" }.into(),
                        at: slot.turn.started_at,
                    }
                } else {
                    ardur_session_journals::JournalEntry::CostFinalized {
                        reservation_id,
                        actual: application.applied_debit,
                        refunded: ardur_cost_gate::CostDelta::full_credit(
                            &application.reserved_credit,
                        ),
                        at: slot.turn.started_at,
                        reason: if matches!(
                            round.decision,
                            Some(SettlementDecision::Completion { .. })
                        ) {
                            None
                        } else {
                            Some(
                                if round
                                    .tools
                                    .iter()
                                    .any(|t| t.effect == ToolEffect::InterruptedUnknown)
                                    && matches!(
                                        round.decision,
                                        Some(SettlementDecision::InfrastructureFailure(_))
                                    )
                                {
                                    "refusal:tool_timeout_uncertain_effect"
                                } else {
                                    projection_reason(round.decision.as_ref())
                                }
                                .into(),
                            )
                        },
                    }
                };
                // Retain before polling a potentially arbitrary journal backend.
                slot.projection_attempts.insert(id, entry.clone());
                entry
            };
            let result = journal.append_settlement(id, entry).await;
            let mut handback = self.0.take(turn)?;
            let slot = handback.slot.as_mut().expect("owned slot");
            let round = slot
                .turn
                .rounds
                .get_mut(ordinal as usize)
                .ok_or(SettlementError::State)?;
            if round.projection != JournalProjection::Pending(id) {
                return Err(SettlementError::State);
            }
            match result {
                ardur_session_journals::ProjectionOutcome::Durable(_) => {
                    round.projection = JournalProjection::Acknowledged(id);
                    slot.persist(&self.0)?;
                    slot.projection_attempts.remove(&id);
                    count += 1;
                }
                ardur_session_journals::ProjectionOutcome::DefinitelyNotApplied(_) => {
                    slot.projection_attempts.remove(&id);
                    return Err(SettlementError::ProjectionNotApplied);
                }
                ardur_session_journals::ProjectionOutcome::Unknown(_) => {
                    slot.error = Some(SettlementError::ProjectionUnknown);
                    return Err(SettlementError::ProjectionUnknown);
                }
            }
        }
        Ok(count)
    }
    /// Refuse unsafe teardown by returning the still-owning supervisor.
    pub fn try_close(self) -> Result<(), Self> {
        self.stop_admission();
        let safe = {
            let s = self.0.state.lock();
            s.executing.is_none()
                && s.busy.is_none()
                && s.slots.is_empty()
                && s.boot_problem.is_none()
        };
        if safe { Ok(()) } else { Err(self) }
    }
}
struct Pending {
    previous: Option<u64>,
    encoded: EncodedSnapshot,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ClaimValidation {
    Unchecked,
    Valid,
    Invalid,
}
struct Slot {
    expected_holder: HolderId,
    validation: ClaimValidation,
    turn: TurnObligation,
    token: Option<SettlementReservation>,
    unclaimed: Option<Reservation>,
    admission_attempt: Option<AdmissionAttempt>,
    view: Option<OwnedBudgetView>,
    acknowledged: Option<EncodedSnapshot>,
    pending: Option<Pending>,
    dirty: bool,
    error: Option<SettlementError>,
    requested: Option<CostTuple>,
    // Reserved before entering admission; installed in durable facts on decision.
    projection_ordinal: Option<u64>,
    finalize_started: bool,
    retirement_pending: bool,
    rejected: Option<Arc<RejectedObservation>>,
    projection_attempts: HashMap<Uuid, ardur_session_journals::JournalEntry>,
}
struct State {
    slots: HashMap<TurnId, Option<Slot>>,
    inventory: HashSet<TurnId>,
    executing: Option<TurnId>,
    busy: Option<TurnId>,
    busy_status: Option<TurnSettlementStatus>,
    stopped: bool,
    boot_problem: Option<SettlementProblem>,
    next_ordinal: u64,
    abandoned: HashSet<TurnId>,
    draining: bool,
}
impl State {
    fn reap(&mut self, id: TurnId) {
        if self.executing == Some(id) || self.abandoned.contains(&id) {
            return;
        }
        let removable = self
            .slots
            .get(&id)
            .and_then(Option::as_ref)
            .is_some_and(|slot| {
                slot.outcome().is_ok()
                    && slot.pending.is_none()
                    && !slot.dirty
                    && !slot.retirement_pending
                    && slot.unclaimed.is_none()
                    && (slot.view.is_some() || slot.unused())
                    && slot.view.as_ref().is_none_or(|v| {
                        matches!(
                            v.status,
                            OwnedBudgetStatus::Released
                                | OwnedBudgetStatus::RolledBack
                                | OwnedBudgetStatus::Committed
                        )
                    })
                    && !slot
                        .turn
                        .rounds
                        .iter()
                        .any(|r| matches!(r.projection, JournalProjection::Pending(_)))
            });
        if removable {
            if let Some(Some(slot)) = self.slots.remove(&id) {
                if slot.acknowledged.is_none() {
                    self.inventory.remove(&id);
                }
            }
        }
    }
}
#[cfg(test)]
struct DrainPause {
    entered: std::sync::mpsc::Sender<Vec<TurnId>>,
    resume: std::sync::mpsc::Receiver<()>,
}
/// Concrete process-scoped owner of a gate, leased store and bounded registry.
pub struct SettlementCoordinator {
    gate: Arc<InMemoryCostAdmissionGate<SharedBudget>>,
    root: std::path::PathBuf,
    identity: ReceiptIdentity,
    store: Mutex<FileSettlementStore>,
    epoch: BudgetEpoch,
    #[cfg(feature = "test-support")]
    pub(crate) test_storage: Option<tempfile::TempDir>,
    encoded_record_probe_bytes: usize,
    state: Mutex<State>,
    #[cfg(test)]
    ack_delivery_fault: Mutex<Option<(TurnId, u64, std::path::PathBuf)>>,
    #[cfg(test)]
    drain_pause: Mutex<Option<DrainPause>>,
}
impl SettlementCoordinator {
    /// Obtain an external retaining supervisor before handing a turn to work.
    pub fn supervisor(self: &Arc<Self>) -> SettlementSupervisor {
        SettlementSupervisor(self.clone())
    }
    /// Inspect safe typed facts, including the pre-operation view while busy.
    /// This acquires no gate/store lock and is safe from injected clock callbacks.
    pub fn status(&self) -> SettlementStatus {
        let state = self.state.lock();
        let mut turns: Vec<_> = state.slots.values().flatten().map(Slot::status).collect();
        turns.extend(state.busy_status.clone());
        for turn in &mut turns {
            turn.abandonment_pending = state.abandoned.contains(&turn.turn.turn_id);
        }
        turns.sort_by_key(|s| s.turn.turn_id.0);
        SettlementStatus {
            epoch: self.epoch,
            executing: state.executing,
            busy: state.busy,
            turns,
            inventory_count: state.inventory.len(),
            encoded_record_probe_bytes: self.encoded_record_probe_bytes,
            stopped: state.stopped,
            boot_problem: state.boot_problem,
        }
    }
    /// Explicitly stop admission, without erasing any pending work.
    pub fn stop_admission(&self) {
        self.state.lock().stopped = true;
    }
    /// Open bounded local evidence and inventory it while holding the writer lease.
    /// Prior-epoch uncertainty closes admission without touching a fresh budget.
    pub fn open(
        gate: Arc<InMemoryCostAdmissionGate<SharedBudget>>,
        root: &Path,
        identity: ReceiptIdentity,
    ) -> Result<Arc<Self>, SettlementError> {
        let encoded_record_probe_bytes = check_record_headroom()?;
        let store = FileSettlementStore::open(root, identity.clone(), LIMITS)
            .map_err(|e| SettlementError::Open(Arc::new(e)))?;
        let inventory = load_settlement_snapshot(root, &identity, &LIMITS)
            .map_err(|e| SettlementError::Open(Arc::new(e)))?;
        let mut ids = HashSet::new();
        let mut boot_problem = None;
        let mut next_ordinal = 0;
        for snapshot in inventory.snapshots {
            let turn = snapshot.turn();
            ids.insert(turn.turn_id);
            if matches!(
                turn.terminal,
                TurnTerminal::Open | TurnTerminal::Unresolved(_)
            ) || !matches!(
                turn.cancellation_marker,
                MarkerProjection::NotRequired | MarkerProjection::Acknowledged(_)
            ) {
                boot_problem = Some(SettlementProblem::PriorEpochApplicationUnknown);
            }
            for round in &turn.rounds {
                if !matches!(round.phase, SettlementPhase::Settled { .. })
                    || matches!(round.projection, JournalProjection::Pending(_))
                {
                    boot_problem = Some(SettlementProblem::PriorEpochApplicationUnknown);
                }
                if let Some(n) = round.commit_ordinal {
                    next_ordinal = next_ordinal.max(
                        n.checked_add(1)
                            .ok_or(SettlementError::Invalid(SettlementProblem::Bounds))?,
                    );
                }
            }
        }
        Ok(Arc::new(Self {
            gate,
            root: root.to_path_buf(),
            identity,
            store: Mutex::new(store),
            epoch: BudgetEpoch(Uuid::new_v4()),
            #[cfg(feature = "test-support")]
            test_storage: None,
            encoded_record_probe_bytes,
            #[cfg(test)]
            ack_delivery_fault: Mutex::new(None),
            #[cfg(test)]
            drain_pause: Mutex::new(None),
            state: Mutex::new(State {
                slots: HashMap::new(),
                inventory: ids,
                executing: None,
                busy: None,
                busy_status: None,
                stopped: false,
                boot_problem,
                next_ordinal,
                abandoned: HashSet::new(),
                draining: false,
            }),
        }))
    }
    /// Reserve a slot AND inventory membership before admitting a hold.
    /// Only one turn may execute. Empty unclaimed slots can be explicitly closed.
    pub fn reserve_turn(
        self: &Arc<Self>,
        identity: TurnIdentity,
    ) -> Result<TurnSettlementOwner, SettlementError> {
        let turn = TurnObligation {
            schema_version: SETTLEMENT_SCHEMA_VERSION,
            turn_id: TurnId(Uuid::new_v4()),
            budget_epoch: self.epoch,
            request_session: identity.request_session,
            journal_owner: identity.journal_owner,
            verified_subject: identity.verified_subject,
            budget_holder: identity.budget_holder,
            cap_token_id: identity.cap_token_id,
            started_at: identity.started_at,
            rounds: Vec::new(),
            terminal: TurnTerminal::Open,
            cancellation_marker: MarkerProjection::NotRequired,
        };
        EncodedSnapshot::new(1, turn.clone(), &LIMITS).map_err(encoding_error)?;
        let mut state = self.state.lock();
        if state.busy.is_some() {
            return Err(SettlementError::Busy);
        }
        if state.stopped
            || state.boot_problem.is_some()
            || !state.abandoned.is_empty()
            || state.slots.values().flatten().any(|s| {
                s.outcome().is_err()
                    || !s.projection_attempts.is_empty()
                    || s.pending.is_some()
                    || s.dirty
                    || s.retirement_pending
                    || s.unclaimed.is_some()
            })
        {
            return Err(SettlementError::AdmissionClosed);
        }
        if state.executing.is_some() {
            return Err(SettlementError::Busy);
        }
        if state.slots.len() >= MAX_SLOTS {
            return Err(SettlementError::SlotsFull);
        }
        if state.inventory.len() >= LIMITS.max_inventory_records {
            return Err(SettlementError::InventoryFull);
        }
        state
            .next_ordinal
            .checked_add(1)
            .ok_or(SettlementError::Invalid(SettlementProblem::Bounds))?;
        let id = turn.turn_id;
        state.inventory.insert(id);
        state.executing = Some(id);
        state.slots.insert(
            id,
            Some(Slot {
                expected_holder: turn.budget_holder.clone(),
                validation: ClaimValidation::Unchecked,
                turn,
                token: None,
                unclaimed: None,
                admission_attempt: None,
                view: None,
                acknowledged: None,
                pending: None,
                dirty: false,
                error: None,
                requested: None,
                projection_ordinal: None,
                finalize_started: false,
                retirement_pending: false,
                rejected: None,
                projection_attempts: HashMap::new(),
            }),
        );
        Ok(TurnSettlementOwner {
            coordinator: self.clone(),
            id,
        })
    }
    fn take(self: &Arc<Self>, id: TurnId) -> Result<Handback, SettlementError> {
        let mut state = self.state.lock();
        if state.busy.is_some() {
            return Err(SettlementError::Busy);
        }
        let slot = state
            .slots
            .get_mut(&id)
            .ok_or(SettlementError::State)?
            .take()
            .ok_or(SettlementError::Busy)?;
        state.busy = Some(id);
        state.busy_status = Some(slot.status());
        Ok(Handback {
            coordinator: self.clone(),
            id,
            slot: Some(slot),
        })
    }
    fn allocate_projection(&self, slot: &mut Slot) -> Result<(), SettlementError> {
        let round = slot.turn.rounds.last_mut().ok_or(SettlementError::State)?;
        if round.commit_ordinal.is_none() {
            round.commit_ordinal = Some(
                slot.projection_ordinal
                    .ok_or(SettlementError::Invalid(SettlementProblem::Bounds))?,
            );
            if slot.turn.journal_owner.is_some() {
                round.projection = JournalProjection::Pending(round.settlement_id.0);
            }
        }
        Ok(())
    }
    // Private per-instance ADAPTER ACK DELIVERY fault, not a filesystem sync
    // failure or a foundation-private seam. The real put_exact already completed.
    #[cfg(test)]
    fn deliver_adapter_ack(
        &self,
        encoded: &EncodedSnapshot,
        resolution: WriteResolution,
    ) -> WriteResolution {
        let mut fault = self.ack_delivery_fault.lock();
        if fault
            .as_ref()
            .is_some_and(|(id, rev, _)| *id == encoded.turn().turn_id && *rev == encoded.revision())
        {
            assert_eq!(resolution, WriteResolution::Durable(encoded.revision()));
            let (_, _, root) = fault.take().unwrap();
            let round = &encoded.turn().rounds[0];
            assert!(
                matches!(round.phase, SettlementPhase::Settled { .. })
                    || (encoded.revision() == 1
                        && matches!(round.phase, SettlementPhase::Reserved))
            );
            let actual =
                std::fs::read(root.join(format!("{}.json", encoded.turn().turn_id.0))).unwrap();
            assert!(
                actual.as_slice() == encoded.bytes(),
                "real durable write must precede withheld adapter acknowledgement"
            );
            assert_eq!(
                ardur_cost_gate::Sha256Digest::of(&actual),
                ardur_cost_gate::Sha256Digest::of(encoded.bytes())
            );
            WriteResolution::Unresolved(SettlementProblem::VerificationFailed)
        } else {
            resolution
        }
    }
    fn drain_abandoned(self: &Arc<Self>) {
        // One bounded pass. Handback reentry must not recurse into cancellation.
        let ids: Vec<_> = {
            let mut state = self.state.lock();
            if state.busy.is_some() || state.draining || std::thread::panicking() {
                return;
            }
            state.draining = true;
            state.abandoned.iter().copied().collect()
        };
        let _draining = AbandonmentDrain(self.clone());
        #[cfg(test)]
        {
            let pause = self.drain_pause.lock().take();
            if let Some(pause) = pause {
                pause.entered.send(ids.clone()).unwrap();
                pause
                    .resume
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            }
        }
        for id in ids {
            let _ = self.cancel(id);
            let mut state = self.state.lock();
            let accounted = state
                .slots
                .get(&id)
                .and_then(Option::as_ref)
                .is_some_and(|s| s.disposition_continuation() || s.unused());
            if accounted {
                // Frozen/selected decisions have their own credit/storage
                // continuations. Unselected abandonment stays independently owned.
                state.abandoned.remove(&id);
                state.reap(id);
            }
        }
    }
    fn cancel(self: &Arc<Self>, id: TurnId) -> Result<(), SettlementError> {
        self.refund_failure_or_cancel(id, None)
    }
    fn refund_failure_or_cancel(
        self: &Arc<Self>,
        id: TurnId,
        failure: Option<InfrastructureFailureClass>,
    ) -> Result<(), SettlementError> {
        let mut handback = self.take(id)?;
        let slot = handback.slot.as_mut().expect("owned slot");
        if matches!(
            slot.error,
            Some(SettlementError::Invalid(_))
                | Some(SettlementError::Storage(
                    SettlementProblem::InvalidTransition
                        | SettlementProblem::Corrupt
                        | SettlementProblem::Bounds
                ))
        ) {
            return Err(SettlementError::AdmissionClosed);
        }
        let round = slot.turn.rounds.last().ok_or(SettlementError::State)?;
        if round.decision == Some(SettlementDecision::Cancelled) {
            slot.outcome()?;
            let exact = slot
                .acknowledged
                .as_ref()
                .is_some_and(|s| s.turn() == &slot.turn);
            if !exact
                || slot.pending.is_some()
                || slot.dirty
                || !matches!(round.phase, SettlementPhase::Settled { receipt: None, .. })
                || slot
                    .view
                    .as_ref()
                    .is_none_or(|v| v.status != OwnedBudgetStatus::Released)
            {
                slot.dirty |= !exact;
                slot.error = Some(SettlementError::Invalid(
                    SettlementProblem::InvalidTransition,
                ));
                return Err(SettlementError::AdmissionClosed);
            }
            return Ok(());
        }
        if round.decision
            == Some(SettlementDecision::Completion {
                final_answer: false,
            })
            && matches!(round.phase, SettlementPhase::Settled { .. })
            && slot.turn.terminal == TurnTerminal::Open
        {
            slot.turn.terminal = TurnTerminal::Cancelled;
            return slot.persist(self);
        }
        if round.decision.is_some() {
            return Err(SettlementError::State);
        }
        if let Err(error) = self.allocate_projection(slot) {
            slot.error = Some(error.clone());
            return Err(error);
        }
        let round = slot.turn.rounds.last_mut().expect("checked round");
        round.decision = Some(failure.map_or(
            SettlementDecision::Cancelled,
            SettlementDecision::InfrastructureFailure,
        ));
        if let ProviderEvidence::Observed {
            finished: false,
            interrupted,
            ..
        } = &mut round.provider_evidence
        {
            *interrupted = true;
        }
        for tool in &mut round.tools {
            if tool.effect == ToolEffect::DispatchIntent {
                tool.effect = ToolEffect::InterruptedUnknown;
            }
        }
        round.phase = SettlementPhase::Prepared { candidate: None };
        slot.turn.terminal = failure.map_or(TurnTerminal::Cancelled, TurnTerminal::Failed);
        // An unavailable snapshot cannot convert precommit cancellation to a
        // caller charge. Retain the immutable pending attempt and newer facts.
        let before = slot.persist(self);
        let known = slot
            .turn
            .rounds
            .last_mut()
            .expect("current round")
            .known_incurred;
        let result = self
            .gate
            .release_owned_sync(slot.token.as_mut().ok_or(SettlementError::State)?, known);
        slot.view = Some(
            self.gate
                .owned_view(slot.token.as_ref().expect("claimed"))
                .map_err(|e| SettlementError::Budget(Arc::new(e)))?,
        );
        slot.record_release(self.epoch);
        if let Err(error) = result {
            let error = SettlementError::Budget(Arc::new(error));
            slot.error = Some(error.clone());
            return Err(error);
        }
        before?;
        slot.persist(self)?;
        slot.outcome()
    }
}
// A sizing probe only, never stored and never represented as an authentic JWS
// or actual work. Reserve a full max_record_bytes quota per membership before
// admitting work. All escaping is maximal for permitted (non-control) metadata.
// This deliberately sizes five rounds even though this first owner slice only
// admits one; receipt/marker capacity is not spendable on additional metadata.
fn check_record_headroom() -> Result<usize, SettlementError> {
    use ardur_cost_gate::Sha256Digest;
    use ardur_session_journals::ReceiptId;
    let tuple = |n| CostTuple {
        tokens_in: n,
        tokens_out: n,
        cents: n,
        wall_ms: n,
        attention_score: n,
    };
    let maximum = tuple(u64::MAX);
    let share = tuple(1_000_000_000_000_000_000);
    let tools = (0..LIMITS.max_tools_per_round)
        .try_fold(CostTuple::ZERO, |sum, _| sum.checked_add(&share))
        .ok_or(SettlementError::Invalid(SettlementProblem::Bounds))?;
    let provider_cost = maximum
        .checked_sub(&tools)
        .ok_or(SettlementError::Invalid(SettlementProblem::Bounds))?;
    let label = "\"".repeat(LIMITS.max_metadata_bytes);
    let text = format!(
        "a.{}.a",
        "a".repeat(
            LIMITS
                .max_receipt_bytes
                .checked_sub(4)
                .ok_or(SettlementError::Invalid(SettlementProblem::Bounds))?
        )
    );
    let candidate = ReceiptCandidate {
        receipt_id: ReceiptId(Uuid::new_v4()),
        expected_parent: Some(Sha256Digest::of(b"capacity")),
        expected_log_end: u64::MAX,
        jws_digest: Sha256Digest::of(text.as_bytes()),
        jws_compact: text,
    };
    let epoch = BudgetEpoch(Uuid::new_v4());
    let mut turn = TurnObligation {
        schema_version: SETTLEMENT_SCHEMA_VERSION,
        turn_id: TurnId(Uuid::new_v4()),
        budget_epoch: epoch,
        request_session: SessionId(Uuid::new_v4()),
        journal_owner: Some(SessionId(Uuid::new_v4())),
        verified_subject: HolderId(label.clone()),
        budget_holder: HolderId(label.clone()),
        cap_token_id: TokenId(Uuid::new_v4()),
        started_at: UnixTsMillis(u64::MAX),
        rounds: Vec::new(),
        terminal: TurnTerminal::Unresolved(SettlementProblem::PriorEpochApplicationUnknown),
        cancellation_marker: MarkerProjection::Prepared(candidate.clone()),
    };
    for index in 0..LIMITS.max_rounds {
        turn.rounds.push(RoundObligation {
            settlement_id: SettlementId(Uuid::new_v4()),
            ordinal: index as u32,
            provider_request_id: Uuid::new_v4(),
            provider: label.clone(),
            model: label.clone(),
            request_digest: Sha256Digest::of(b"capacity"),
            reserved: maximum,
            provider_evidence: ProviderEvidence::Observed {
                usage: Some(UsageSnapshot {
                    input_tokens: u64::MAX,
                    output_tokens: u64::MAX,
                }),
                cost: provider_cost,
                provenance: CostProvenance::PricedUsage(Sha256Digest::of(b"capacity")),
                finished: false,
                interrupted: false,
            },
            tools: (0..LIMITS.max_tools_per_round)
                .map(|n| ToolEvidence {
                    ordinal: n as u32,
                    call_id: label.clone(),
                    name: label.clone(),
                    arguments_digest: Sha256Digest::of(b"capacity"),
                    effect: ToolEffect::Completed {
                        output_digest: Sha256Digest::of(b"capacity"),
                        cost: share,
                    },
                    output_admission: OutputAdmission::NotScanned,
                })
                .collect(),
            known_incurred: maximum,
            decision: Some(SettlementDecision::InfrastructureFailure(
                InfrastructureFailureClass::Provider,
            )),
            phase: SettlementPhase::Unresolved {
                last_definite: DefinitePhase::WorkObserved,
                problem: SettlementProblem::PriorEpochApplicationUnknown,
                application: Some(DebitApplication {
                    epoch,
                    requested_debit: maximum,
                    applied_debit: maximum,
                    reserved_credit: tuple(u64::MAX - 1),
                    additional_debit: tuple(u64::MAX - 1),
                    shortfall: CostTuple::ZERO,
                    rollback: RollbackStatus::Applied(maximum),
                }),
                candidate: Some(candidate.clone()),
            },
            commit_ordinal: Some(u64::MAX),
            projection: JournalProjection::Acknowledged(Uuid::new_v4()),
        });
    }
    let encoded = EncodedSnapshot::new(u64::MAX, turn, &LIMITS).map_err(encoding_error)?;
    // Extra space covers wider mutually constrained numeric combinations and
    // enum discriminants, beyond the actual escaped/candidate/application probe.
    if encoded
        .bytes()
        .len()
        .checked_add(16_384)
        .is_none_or(|n| n > LIMITS.max_record_bytes)
    {
        return Err(SettlementError::Invalid(SettlementProblem::Bounds));
    }
    Ok(encoded.bytes().len())
}
fn projection_reason(decision: Option<&SettlementDecision>) -> &'static str {
    match decision {
        Some(SettlementDecision::Refusal(c)) => match c {
            RefusalClass::UnknownTool => "refusal:unknown_tool",
            RefusalClass::Authorization => "refusal:tool_authorization",
            RefusalClass::MissingCapability => "refusal:capability_denied",
            RefusalClass::ApprovalRequired => "refusal:approval_required",
            RefusalClass::ApprovalRejected => "refusal:approval_rejected",
            RefusalClass::ApprovalError => "refusal:approval_evaluation_error",
            RefusalClass::OutputBlocked => "refusal:output_scan",
            RefusalClass::ScannerError => "refusal:output_scan_error",
            RefusalClass::IterationLimit => "refusal:iteration_limit",
            RefusalClass::Capacity => "refusal:capacity",
        },
        Some(SettlementDecision::InfrastructureFailure(_)) => "refusal:tool_error",
        Some(SettlementDecision::Cancelled) => {
            "settlement:cancelled_zero_debit_unknown_expense_not_zero"
        }
        _ => "settlement:completion",
    }
}
fn positive_difference(a: CostTuple, b: CostTuple) -> CostTuple {
    CostTuple {
        tokens_in: a.tokens_in.saturating_sub(b.tokens_in),
        tokens_out: a.tokens_out.saturating_sub(b.tokens_out),
        cents: a.cents.saturating_sub(b.cents),
        wall_ms: a.wall_ms.saturating_sub(b.wall_ms),
        attention_score: a.attention_score.saturating_sub(b.attention_score),
    }
}
fn encoding_error(error: SettlementStoreError) -> SettlementError {
    SettlementError::Invalid(match error {
        SettlementStoreError::Invalid(p) => p,
        _ => SettlementProblem::Corrupt,
    })
}
impl Slot {
    fn disposition_continuation(&self) -> bool {
        let Some(round) = self.turn.rounds.last() else {
            return false;
        };
        match round.decision {
            Some(SettlementDecision::Cancelled) => self.view.as_ref().is_some_and(|v| {
                matches!(
                    v.status,
                    OwnedBudgetStatus::Released | OwnedBudgetStatus::ReleasePending
                ) && v.refund.is_some()
            }),
            Some(
                SettlementDecision::Refusal(_)
                | SettlementDecision::InfrastructureFailure(_)
                | SettlementDecision::Completion { .. },
            ) => {
                // A frozen paid decision is not a refund instruction. It needs
                // acknowledged selection or retained exact storage continuation,
                // not merely a locally assigned enum value.
                let selected = |snapshot: &EncodedSnapshot| {
                    snapshot.turn().rounds.last().is_some_and(|r| {
                        r.settlement_id == round.settlement_id
                            && r.decision == round.decision
                            && r.commit_ordinal.is_some()
                            && r.commit_ordinal == round.commit_ordinal
                    })
                };
                self.acknowledged.as_ref().is_some_and(selected)
                    || self.pending.as_ref().is_some_and(|p| selected(&p.encoded))
            }
            _ => false,
        }
    }
    fn retire_acknowledged(
        &mut self,
        coordinator: &SettlementCoordinator,
    ) -> Result<(), SettlementError> {
        if !self.retirement_pending {
            return Ok(());
        }
        // Durable evidence is not the capability. Retire only the already
        // finalized token after exact acknowledgement of this supported decision.
        if self.validation != ClaimValidation::Valid
            || self.pending.is_some()
            || self.dirty
            || self.error.is_some()
            || self
                .acknowledged
                .as_ref()
                .is_none_or(|s| s.turn() != &self.turn)
            || self.turn.rounds.last().is_none_or(|r| {
                !matches!(r.phase, SettlementPhase::Settled { .. })
                    || !matches!(
                        r.decision,
                        Some(
                            SettlementDecision::Refusal(_)
                                | SettlementDecision::InfrastructureFailure(_)
                                | SettlementDecision::Completion { .. }
                        )
                    )
            })
        {
            return Err(SettlementError::AdmissionClosed);
        }
        let result = coordinator
            .gate
            .commit_owned_sync(self.token.as_mut().ok_or(SettlementError::State)?);
        self.refresh(coordinator)?;
        if let Err(e) = result {
            let error = SettlementError::Budget(Arc::new(e));
            self.error = Some(error.clone());
            return Err(error);
        }
        if self
            .view
            .as_ref()
            .is_some_and(|v| v.status == OwnedBudgetStatus::Committed)
        {
            self.retirement_pending = false;
            Ok(())
        } else {
            Err(SettlementError::State)
        }
    }
    fn unused(&self) -> bool {
        self.admission_attempt.is_none()
            && self.token.is_none()
            && self.unclaimed.is_none()
            && self.view.is_none()
            && self.turn.rounds.is_empty()
            && self.error.is_none()
            && self.acknowledged.is_none()
            && self.pending.is_none()
            && !self.dirty
            && self.rejected.is_none()
    }
    fn status(&self) -> TurnSettlementStatus {
        TurnSettlementStatus {
            turn: self.turn.clone(),
            pending_projections: self
                .projection_attempts
                .iter()
                .map(|(id, entry)| (*id, entry.clone()))
                .collect(),
            view: self.view.clone(),
            error: self.outcome().err(),
            expected_holder: self.expected_holder.clone(),
            validation_error: (self.validation == ClaimValidation::Invalid)
                .then_some(SettlementError::IdentityMismatch),
            acknowledged_revision: self.acknowledged.as_ref().map(EncodedSnapshot::revision),
            pending_revision: self.pending.as_ref().map(|p| p.encoded.revision()),
            pending_digest: self.pending.as_ref().map(|p| p.encoded.digest()),
            undurable: self.dirty,
            unclaimed_reservation: self.unclaimed.as_ref().map(|r| r.reservation_id),
            admission_attempt: self.admission_attempt.clone(),
            rejected: self.rejected.clone(),
            abandonment_pending: false, // coordinator overlays even a busy slot
            retirement_pending: self.retirement_pending,
        }
    }
    fn refresh(&mut self, coordinator: &SettlementCoordinator) -> Result<(), SettlementError> {
        self.view = Some(
            coordinator
                .gate
                .owned_view(self.token.as_ref().ok_or(SettlementError::State)?)
                .map_err(|e| SettlementError::Budget(Arc::new(e)))?,
        );
        Ok(())
    }
    fn record_application(&mut self, epoch: BudgetEpoch) {
        let candidate = match &self.turn.rounds.last().expect("current round").phase {
            SettlementPhase::Prepared { candidate }
            | SettlementPhase::Finalized { candidate, .. }
            | SettlementPhase::Unresolved { candidate, .. } => candidate.clone(),
            _ => None,
        };
        let view = self.view.as_ref().expect("live view");
        if let (Some(attempt), Some(actual)) = (&view.attempt, &view.application) {
            let mut application = DebitApplication {
                epoch,
                requested_debit: attempt.requested_debit,
                applied_debit: actual.applied_debit,
                reserved_credit: actual.reserved_credit,
                additional_debit: actual.additional_debit,
                shortfall: actual.shortfall,
                rollback: RollbackStatus::None,
            };
            let complete_credit = application.reserved_credit.covers(&positive_difference(
                view.reserved,
                application.requested_debit,
            ));
            self.turn.rounds.last_mut().expect("current round").phase = if matches!(
                view.status,
                OwnedBudgetStatus::RollbackPending | OwnedBudgetStatus::RolledBack
            ) {
                application.rollback = RollbackStatus::Applied(
                    view.refund.as_ref().expect("real refund").applied_credit,
                );
                if view.status == OwnedBudgetStatus::RolledBack {
                    if matches!(self.error, Some(SettlementError::CreditPending(_))) {
                        self.error = None;
                    }
                    self.turn.terminal =
                        match self.turn.rounds.last_mut().expect("current round").decision {
                            Some(SettlementDecision::Refusal(c)) => TurnTerminal::Refused(c),
                            Some(SettlementDecision::InfrastructureFailure(c)) => {
                                TurnTerminal::Failed(c)
                            }
                            _ => self.turn.terminal.clone(),
                        };
                    SettlementPhase::Settled {
                        application,
                        receipt: None,
                    }
                } else {
                    if !matches!(self.error, Some(SettlementError::Storage(_))) {
                        self.error = Some(SettlementError::CreditPending(
                            SettlementProblem::RollbackCreditPending,
                        ));
                    }
                    SettlementPhase::Unresolved {
                        last_definite: DefinitePhase::Finalized,
                        problem: SettlementProblem::RollbackCreditPending,
                        application: Some(application),
                        candidate: candidate.clone(),
                    }
                }
            } else if complete_credit {
                SettlementPhase::Finalized {
                    application,
                    candidate: candidate.clone(),
                }
            } else {
                if !matches!(self.error, Some(SettlementError::Storage(_))) {
                    self.error = Some(SettlementError::CreditPending(
                        SettlementProblem::ReleaseCreditPending,
                    ));
                }
                SettlementPhase::Unresolved {
                    last_definite: DefinitePhase::Finalized,
                    problem: SettlementProblem::ReleaseCreditPending,
                    application: Some(application),
                    candidate: candidate.clone(),
                }
            };
            self.dirty = true;
        }
    }
    fn persist(&mut self, coordinator: &SettlementCoordinator) -> Result<(), SettlementError> {
        self.dirty = true;
        if self.pending.is_some() {
            return Err(self
                .error
                .clone()
                .unwrap_or(SettlementError::Storage(SettlementProblem::Unhealthy)));
        }
        let previous = self.acknowledged.as_ref().map(EncodedSnapshot::revision);
        let revision = previous
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(SettlementError::Invalid(SettlementProblem::Bounds))?;
        let encoded = match EncodedSnapshot::new(revision, self.turn.clone(), &LIMITS) {
            Ok(encoded) => encoded,
            Err(e) => {
                let error = encoding_error(e);
                self.error = Some(error.clone());
                return Err(error);
            }
        };
        self.pending = Some(Pending { previous, encoded });
        let pending = self.pending.as_ref().expect("retained before write");
        let resolution = coordinator
            .store
            .lock()
            .put_exact(pending.previous, &pending.encoded);
        #[cfg(test)]
        let resolution = coordinator.deliver_adapter_ack(&pending.encoded, resolution);
        self.acknowledge(resolution)
    }
    fn acknowledge(&mut self, resolution: WriteResolution) -> Result<(), SettlementError> {
        if let WriteResolution::Durable(revision) = resolution {
            if self
                .pending
                .as_ref()
                .is_some_and(|p| p.encoded.revision() == revision)
            {
                let pending = self.pending.take().expect("matched");
                self.dirty = pending.encoded.turn() != &self.turn;
                self.acknowledged = Some(pending.encoded);
                if matches!(self.error, Some(SettlementError::Storage(_))) {
                    self.error = None;
                }
                return Ok(());
            }
        }
        let problem = match resolution {
            WriteResolution::Unresolved(p) => p,
            _ => SettlementProblem::VerificationFailed,
        };
        let error = SettlementError::Storage(problem);
        self.error = Some(error.clone());
        Err(error)
    }
    fn outcome(&self) -> Result<(), SettlementError> {
        match &self.error {
            Some(e) => Err(e.clone()),
            None if self.validation == ClaimValidation::Invalid => {
                Err(SettlementError::IdentityMismatch)
            }
            None => Ok(()),
        }
    }
    fn record_release(&mut self, epoch: BudgetEpoch) {
        let view = self.view.as_ref().expect("live view");
        if let Some(refund) = &view.refund {
            let application = DebitApplication {
                epoch,
                requested_debit: CostTuple::ZERO,
                applied_debit: view
                    .reserved
                    .checked_sub(&refund.applied_credit)
                    .expect("actual bounded credit"),
                reserved_credit: refund.applied_credit,
                additional_debit: CostTuple::ZERO,
                shortfall: CostTuple::ZERO,
                rollback: RollbackStatus::None,
            };
            self.turn.rounds.last_mut().expect("current round").phase =
                if view.status == OwnedBudgetStatus::Released {
                    if matches!(self.error, Some(SettlementError::CreditPending(_))) {
                        self.error = None;
                    }
                    SettlementPhase::Settled {
                        application,
                        receipt: None,
                    }
                } else {
                    if !matches!(self.error, Some(SettlementError::Storage(_))) {
                        self.error = Some(SettlementError::CreditPending(
                            SettlementProblem::ReleaseCreditPending,
                        ));
                    }
                    SettlementPhase::Unresolved {
                        last_definite: DefinitePhase::Finalized,
                        problem: SettlementProblem::ReleaseCreditPending,
                        application: Some(application),
                        candidate: None,
                    }
                };
            self.dirty = true;
        }
    }
}
// A stack guard, not an owned mutex guard. No coordinator/store locks cross a
// gate call. An injected clock may reenter or unwind and still hand authority back.
struct Handback {
    coordinator: Arc<SettlementCoordinator>,
    id: TurnId,
    slot: Option<Slot>,
}
impl Drop for Handback {
    fn drop(&mut self) {
        if let Some(mut slot) = self.slot.take() {
            if let Some(token) = &slot.token {
                if let Ok(view) = self.coordinator.gate.owned_view(token) {
                    slot.view = Some(view);
                }
            }
            if std::thread::panicking() {
                slot.error = Some(SettlementError::Panicked);
            }
            let mut state = self.coordinator.state.lock();
            state.slots.insert(self.id, Some(slot));
            state.busy = None;
            state.busy_status = None;
            state.reap(self.id);
            drop(state);
            self.coordinator.drain_abandoned();
        }
    }
}
struct AbandonmentDrain(Arc<SettlementCoordinator>);
impl Drop for AbandonmentDrain {
    fn drop(&mut self) {
        self.0.state.lock().draining = false;
    }
}
/// A turn-lifetime handle retaining the coordinator without an Arc cycle.
pub struct TurnSettlementOwner {
    coordinator: Arc<SettlementCoordinator>,
    id: TurnId,
}
impl Drop for TurnSettlementOwner {
    fn drop(&mut self) {
        // Concrete release_owned_sync has no clock/hook callback and performs no
        // asynchronous work. A frozen decision is never relabelled cancellation.
        let mut state = self.coordinator.state.lock();
        if state.slots.contains_key(&self.id) {
            // Retain disappearance BEFORE releasing execution, even Busy.
            state.abandoned.insert(self.id);
        }
        if state.executing == Some(self.id) {
            state.executing = None;
        }
        state.reap(self.id);
        drop(state);
        self.coordinator.drain_abandoned();
    }
}
impl TurnSettlementOwner {
    /// Stable turn ID, allocated once when capacity is reserved.
    pub fn turn_id(&self) -> TurnId {
        self.id
    }
    /// Admit and immediately claim a bounded round using the concrete gate.
    /// Continuation requires the preceding capability to be durably committed.
    /// The original request token is checked before admission; only the live
    /// view supplies the actual holder/reserve. No mutable Reservation is accepted.
    pub async fn admit_round(
        &mut self,
        request: AdmissionRequest,
        provider_request_id: Uuid,
    ) -> Result<(), SettlementError> {
        let mut handback = self.coordinator.take(self.id)?;
        let slot = handback.slot.as_mut().expect("owned slot");
        {
            let state = self.coordinator.state.lock();
            if state.stopped
                || state.boot_problem.is_some()
                || !state.abandoned.is_empty()
                || state.slots.values().flatten().any(|s| {
                    s.outcome().is_err()
                        || !s.projection_attempts.is_empty()
                        || s.pending.is_some()
                        || s.dirty
                        || s.retirement_pending
                        || s.unclaimed.is_some()
                })
            {
                return Err(SettlementError::AdmissionClosed);
            }
        }
        if slot.turn.rounds.len() >= LIMITS.max_rounds {
            return Err(SettlementError::Invalid(SettlementProblem::Bounds));
        }
        if !slot.turn.rounds.is_empty()
            && slot.turn.terminal == TurnTerminal::Open
            && slot
                .view
                .as_ref()
                .is_some_and(|v| v.status == OwnedBudgetStatus::Committed)
            && slot.pending.is_none()
            && !slot.dirty
            && !slot.retirement_pending
            && slot.error.is_none()
        {
            slot.token = None;
            slot.view = None;
            slot.admission_attempt = None;
            slot.requested = None;
            slot.finalize_started = false;
        }
        if slot.turn.terminal != TurnTerminal::Open
            || slot.error.is_some()
            || slot.token.is_some()
            || slot.unclaimed.is_some()
            || slot.admission_attempt.is_some()
        {
            return Err(SettlementError::State);
        }
        if request.cap_token_id != slot.turn.cap_token_id {
            return Err(SettlementError::IdentityMismatch);
        }
        let mut round = RoundObligation {
            settlement_id: SettlementId(Uuid::new_v4()),
            ordinal: slot.turn.rounds.len() as u32,
            provider_request_id,
            provider: request.provider_id.0.clone(),
            model: request.model_id.0.clone(),
            request_digest: request.request_digest,
            reserved: CostTuple::from_envelope(&request.projected_envelope),
            provider_evidence: ProviderEvidence::NotDispatched,
            tools: Vec::new(),
            known_incurred: CostTuple::ZERO,
            decision: None,
            phase: SettlementPhase::Reserved,
            commit_ordinal: None,
            projection: if slot.turn.journal_owner.is_some() {
                JournalProjection::NotRequired
            } else {
                JournalProjection::NotConfigured
            },
        };
        let mut preflight = slot.turn.clone();
        preflight.rounds.push(round.clone());
        EncodedSnapshot::new(1, preflight, &LIMITS).map_err(encoding_error)?;
        // One executing owner and a Busy handback serialize this reservation.
        // Never wrap/reuse ordinals, including after uncertain admission.
        {
            let mut state = self.coordinator.state.lock();
            let next = state
                .next_ordinal
                .checked_add(1)
                .ok_or(SettlementError::Invalid(SettlementProblem::Bounds))?;
            slot.projection_ordinal = Some(state.next_ordinal);
            state.next_ordinal = next;
        }
        // SharedBudget's futures complete synchronously. There is no arbitrary
        // budget backend or suspension between successful admission and claim.
        // Absence of a returned handle is not proof of no economic effect.
        // Once the concrete gate is entered, retain uncertainty even on unwind.
        slot.admission_attempt = Some(AdmissionAttempt {
            request: request.clone(),
            provider_request_id,
        });
        let reservation = self.coordinator.gate.admit(request).await.map_err(|e| {
            // These concrete gate verdicts precede successful reserve. In
            // SharedBudget, RaceLost also leaves the account unchanged. Only
            // this admission boundary can discharge the unused attempt; a
            // failed claim below already owns a real returned reservation.
            let unused = matches!(
                e,
                AdmissionError::BudgetExhausted { .. }
                    | AdmissionError::CapTokenInvalid
                    | AdmissionError::ProviderNotAllowed(_)
                    | AdmissionError::PolicyDenied(_)
            );
            let error = SettlementError::Budget(Arc::new(e));
            if unused {
                slot.admission_attempt = None;
                // The coordinator's next_ordinal remains advanced: never reuse
                // an identity even when admission definitely did no work.
                slot.projection_ordinal = None;
            } else {
                slot.error = Some(error.clone());
            }
            error
        })?;
        slot.unclaimed = Some(reservation);
        let token = self
            .coordinator
            .gate
            .claim_owned(slot.unclaimed.as_ref().expect("admitted"))
            .map_err(|e| {
                let error = SettlementError::Budget(Arc::new(e));
                slot.error = Some(error.clone());
                error
            })?;
        slot.token = Some(token);
        slot.unclaimed = None;
        let view = self
            .coordinator
            .gate
            .owned_view(slot.token.as_ref().expect("claimed"))
            .map_err(|e| SettlementError::Budget(Arc::new(e)))?;
        round.settlement_id = SettlementId(view.reservation_id);
        round.reserved = view.reserved;
        // Freeze validation BEFORE persistence; I/O recovery cannot validate a
        // rejected claim. Durable attribution still records the actual holder.
        slot.validation = if slot.expected_holder == view.holder {
            ClaimValidation::Valid
        } else {
            ClaimValidation::Invalid
        };
        slot.turn.budget_holder = view.holder.clone();
        slot.view = Some(view);
        slot.turn.rounds.push(round);
        slot.persist(&self.coordinator)?;
        slot.outcome()
    }
    /// Persist cumulative provider evidence before further work. No payload text.
    pub fn observe_provider(&mut self, evidence: ProviderEvidence) -> Result<(), SettlementError> {
        self.observe(RejectedObservation::Provider(evidence))
    }
    /// Persist a trusted tool result/intent. Invalid evidence remains owned.
    /// The embedder verifies late results; this API does not authenticate effects.
    pub fn observe_tool(&mut self, evidence: ToolEvidence) -> Result<(), SettlementError> {
        self.observe(RejectedObservation::Tool(evidence))
    }
    fn observe(&mut self, evidence: RejectedObservation) -> Result<(), SettlementError> {
        let mut handback = self.coordinator.take(self.id)?;
        let slot = handback.slot.as_mut().expect("owned slot");
        if slot.validation != ClaimValidation::Valid
            || slot.pending.is_some()
            || slot.error.is_some()
            || slot.rejected.is_some()
        {
            return Err(SettlementError::AdmissionClosed);
        }
        if slot
            .turn
            .rounds
            .last()
            .ok_or(SettlementError::State)?
            .decision
            .is_some()
        {
            return Err(SettlementError::State);
        }
        // Retain the moved original before encoding, aggregation or store calls.
        // Oversized labels are rejected before cloning their allocation.
        slot.rejected = Some(Arc::new(evidence));
        slot.dirty = true;
        let evidence = slot.rejected.as_deref().expect("retained");
        let mut next = slot.turn.clone();
        let round = next.rounds.last_mut().expect("round checked");
        match evidence {
            RejectedObservation::Provider(provider) => round.provider_evidence = provider.clone(),
            RejectedObservation::Tool(tool) => {
                if tool.call_id.len() > LIMITS.max_metadata_bytes
                    || tool.name.len() > LIMITS.max_metadata_bytes
                    || tool.ordinal as usize >= LIMITS.max_tools_per_round
                {
                    let error = SettlementError::Invalid(SettlementProblem::Bounds);
                    slot.error = Some(error.clone());
                    return Err(error);
                }
                let index = tool.ordinal as usize;
                if index == round.tools.len() {
                    round.tools.push(tool.clone());
                } else if index < round.tools.len() {
                    round.tools[index] = tool.clone();
                } else {
                    let error = SettlementError::Invalid(SettlementProblem::Corrupt);
                    slot.error = Some(error.clone());
                    return Err(error);
                }
            }
        }
        let mut known = match &round.provider_evidence {
            ProviderEvidence::Observed { cost, .. } => *cost,
            _ => CostTuple::ZERO,
        };
        for tool in &round.tools {
            if let ToolEffect::Completed { cost, .. } = tool.effect {
                let Some(sum) = known.checked_add(&cost) else {
                    let error = SettlementError::Invalid(SettlementProblem::Corrupt);
                    slot.error = Some(error.clone());
                    return Err(error);
                };
                known = sum;
            }
        }
        round.known_incurred = known;
        round.phase = SettlementPhase::WorkObserved;
        slot.turn = next;
        slot.persist(&self.coordinator)?;
        slot.rejected = None;
        Ok(())
    }
    /// Freeze a supported decision and requested debit before money moves.
    pub fn prepare(
        &mut self,
        decision: Disposition,
        requested: CostTuple,
    ) -> Result<(), SettlementError> {
        let mut handback = self.coordinator.take(self.id)?;
        let slot = handback.slot.as_mut().expect("owned slot");
        if slot.validation != ClaimValidation::Valid
            || slot.error.is_some()
            || slot.pending.is_some()
            || slot.dirty
        {
            return Err(SettlementError::AdmissionClosed);
        }
        let round = slot.turn.rounds.last_mut().ok_or(SettlementError::State)?;
        if round.decision.is_some() || !round.known_incurred.covers(&requested) {
            return Err(SettlementError::State);
        }
        // Capacity failure must not install a partial clean decision.
        if let Err(error) = self.coordinator.allocate_projection(slot) {
            slot.error = Some(error.clone());
            return Err(error);
        }
        let round = slot.turn.rounds.last_mut().expect("checked round");
        round.decision = Some(match decision {
            Disposition::Refusal(c) => SettlementDecision::Refusal(c),
            Disposition::Infrastructure(c) => SettlementDecision::InfrastructureFailure(c),
        });
        round.phase = SettlementPhase::Prepared { candidate: None };
        slot.requested = Some(requested);
        slot.persist(&self.coordinator)
    }
    /// Finalize once, preserving exact application evidence.
    pub fn finalize_sync(&mut self) -> Result<(), SettlementError> {
        let mut handback = self.coordinator.take(self.id)?;
        let slot = handback.slot.as_mut().expect("owned slot");
        if slot.finalize_started {
            return Err(SettlementError::State);
        }
        if slot.validation != ClaimValidation::Valid
            || slot.error.is_some()
            || slot.pending.is_some()
            || slot.dirty
        {
            return Err(SettlementError::AdmissionClosed);
        }
        let round = slot.turn.rounds.last().ok_or(SettlementError::State)?;
        if !matches!(round.phase, SettlementPhase::Prepared { .. })
            || !matches!(
                round.decision,
                Some(
                    SettlementDecision::Refusal(_)
                        | SettlementDecision::InfrastructureFailure(_)
                        | SettlementDecision::Completion { .. }
                )
            )
        {
            return Err(SettlementError::State);
        }
        // Clean flags are not durable authority. Require the exact CURRENT
        // Prepared facts acknowledged by the concrete store before entering money.
        if slot
            .acknowledged
            .as_ref()
            .is_none_or(|s| s.turn() != &slot.turn)
        {
            slot.dirty = true;
            slot.error = Some(SettlementError::Invalid(
                SettlementProblem::InvalidTransition,
            ));
            return Err(SettlementError::AdmissionClosed);
        }
        let known = round.known_incurred;
        let requested = slot.requested.ok_or(SettlementError::State)?;
        slot.finalize_started = true;
        let result = self.coordinator.gate.finalize_owned_sync(
            slot.token.as_mut().ok_or(SettlementError::State)?,
            known,
            requested,
        );
        slot.refresh(&self.coordinator)?;
        slot.record_application(self.coordinator.epoch);
        if let Err(error) = result {
            let error = SettlementError::Budget(Arc::new(error));
            slot.error = Some(error.clone());
            return Err(error);
        }
        slot.persist(&self.coordinator)?;
        if let Some(error) = &slot.error {
            return Err(error.clone());
        }
        Ok(())
    }
    /// Durably settle then retire rollback authority. No receipt is appended.
    pub fn commit_disposition(&mut self) -> Result<(), SettlementError> {
        let mut handback = self.coordinator.take(self.id)?;
        let slot = handback.slot.as_mut().expect("owned slot");
        if slot.validation != ClaimValidation::Valid
            || slot.error.is_some()
            || slot.pending.is_some()
            || slot.dirty
        {
            return Err(SettlementError::AdmissionClosed);
        }
        if slot.retirement_pending {
            return slot.retire_acknowledged(&self.coordinator);
        }
        if slot
            .view
            .as_ref()
            .is_some_and(|v| v.status == OwnedBudgetStatus::Committed)
        {
            return Ok(());
        }
        let round = slot.turn.rounds.last_mut().ok_or(SettlementError::State)?;
        let SettlementPhase::Finalized { application, .. } = &round.phase else {
            return Err(SettlementError::State);
        };
        let application = application.clone();
        slot.turn.terminal = match round.decision {
            Some(SettlementDecision::Refusal(c)) => TurnTerminal::Refused(c),
            Some(SettlementDecision::InfrastructureFailure(c)) => TurnTerminal::Failed(c),
            _ => return Err(SettlementError::State),
        };
        round.phase = SettlementPhase::Settled {
            application,
            receipt: None,
        };
        slot.retirement_pending = true;
        slot.persist(&self.coordinator)?;
        slot.retire_acknowledged(&self.coordinator)
    }
    /// Terminalize an already paid intermediate round without calling it a final answer.
    pub(crate) fn finish_refused(&mut self, class: RefusalClass) -> Result<(), SettlementError> {
        let mut handback = self.coordinator.take(self.id)?;
        let slot = handback.slot.as_mut().expect("owned slot");
        slot.outcome()?;
        if slot.turn.terminal != TurnTerminal::Open
            || slot.pending.is_some()
            || slot.dirty
            || !slot
                .turn
                .rounds
                .last()
                .is_some_and(|r| matches!(r.phase, SettlementPhase::Settled { .. }))
        {
            return Err(SettlementError::State);
        }
        slot.turn.terminal = TurnTerminal::Refused(class);
        slot.persist(&self.coordinator)
    }
    pub(crate) fn refund_failure(
        &mut self,
        class: InfrastructureFailureClass,
    ) -> Result<(), SettlementError> {
        self.coordinator
            .refund_failure_or_cancel(self.id, Some(class))
    }
    /// Current reservation identity, without transferring raw authority.
    pub(crate) fn reservation_id(&self) -> Result<Uuid, SettlementError> {
        let state = self.coordinator.state.lock();
        Ok(state
            .slots
            .get(&self.id)
            .and_then(Option::as_ref)
            .and_then(|s| s.view.as_ref())
            .ok_or(SettlementError::State)?
            .reservation_id)
    }
    /// Freeze the exact signed completion candidate before applying money.
    pub(crate) fn prepare_receipt(
        &mut self,
        candidate: ReceiptCandidate,
        final_answer: bool,
        requested: CostTuple,
    ) -> Result<(), SettlementError> {
        let mut handback = self.coordinator.take(self.id)?;
        let slot = handback.slot.as_mut().expect("owned slot");
        slot.outcome()?;
        if slot.pending.is_some() || slot.dirty || slot.validation != ClaimValidation::Valid {
            return Err(SettlementError::AdmissionClosed);
        }
        let round = slot.turn.rounds.last().ok_or(SettlementError::State)?;
        if round.decision.is_some() || requested != round.known_incurred {
            return Err(SettlementError::State);
        }
        self.coordinator.allocate_projection(slot)?;
        let round = slot.turn.rounds.last_mut().expect("current round");
        round.decision = Some(SettlementDecision::Completion { final_answer });
        round.phase = SettlementPhase::Prepared {
            candidate: Some(candidate),
        };
        slot.requested = Some(requested);
        slot.persist(&self.coordinator)
    }
    pub(crate) fn receipt_unresolved(&mut self) -> Result<(), SettlementError> {
        let mut handback = self.coordinator.take(self.id)?;
        let slot = handback.slot.as_mut().expect("owned slot");
        let round = slot.turn.rounds.last_mut().ok_or(SettlementError::State)?;
        let SettlementPhase::Finalized {
            application,
            candidate,
        } = &round.phase
        else {
            return Err(SettlementError::State);
        };
        round.phase = SettlementPhase::Unresolved {
            last_definite: DefinitePhase::Finalized,
            problem: SettlementProblem::Io,
            application: Some(application.clone()),
            candidate: candidate.clone(),
        };
        slot.turn.terminal = TurnTerminal::Unresolved(SettlementProblem::Io);
        slot.persist(&self.coordinator)?;
        slot.error = Some(SettlementError::ReceiptUnknown);
        Ok(())
    }
    /// Called only after the authenticating receipt layer acknowledged exact bytes.
    pub(crate) fn commit_receipt(
        &mut self,
        binding: ReceiptBinding,
    ) -> Result<(), SettlementError> {
        let mut handback = self.coordinator.take(self.id)?;
        let slot = handback.slot.as_mut().expect("owned slot");
        slot.outcome()?;
        if slot.pending.is_some() || slot.dirty {
            return Err(SettlementError::AdmissionClosed);
        }
        let round = slot.turn.rounds.last_mut().ok_or(SettlementError::State)?;
        let SettlementPhase::Finalized {
            application,
            candidate: Some(candidate),
        } = &round.phase
        else {
            return Err(SettlementError::State);
        };
        if binding.receipt_id != candidate.receipt_id || binding.jws_digest != candidate.jws_digest
        {
            return Err(SettlementError::IdentityMismatch);
        }
        let Some(SettlementDecision::Completion { final_answer }) = round.decision else {
            return Err(SettlementError::State);
        };
        if final_answer {
            slot.turn.terminal = TurnTerminal::FinalAnswer(binding.receipt_id);
        }
        round.phase = SettlementPhase::Settled {
            application: application.clone(),
            receipt: Some(binding),
        };
        slot.retirement_pending = true;
        slot.persist(&self.coordinator)?;
        slot.retire_acknowledged(&self.coordinator)?;
        if final_answer {
            let mut state = self.coordinator.state.lock();
            if state.executing == Some(self.id) {
                state.executing = None;
            }
        }
        Ok(())
    }
    /// Synchronously cancel an undecided round; repeated calls do not credit twice.
    /// Pending credit/storage remains an error on repeat. A different frozen
    /// decision returns `State` without relabelling it or changing money.
    pub fn abandon_sync(&mut self) -> Result<(), SettlementError> {
        self.coordinator.cancel(self.id)
    }
    /// Release an unused capacity reservation, without inventing economic evidence.
    pub fn close_empty(self) -> Result<(), SettlementError> {
        let mut state = self.coordinator.state.lock();
        if state.busy.is_some() {
            return Err(SettlementError::Busy);
        }
        let slot = state
            .slots
            .get(&self.id)
            .and_then(Option::as_ref)
            .ok_or(SettlementError::State)?;
        if !slot.unused() {
            return Err(SettlementError::State);
        }
        state.slots.remove(&self.id);
        state.inventory.remove(&self.id);
        state.executing = None;
        Ok(())
    }
}

#[cfg(test)]
mod owner_spec_tests {
    use super::*;
    use ardur_cost_gate::{
        BudgetStore, CostEnvelope, ManualClock, ModelId, ProviderId, Sha256Digest,
    };

    struct Fixture {
        _temp: tempfile::TempDir,
        root: std::path::PathBuf,
        receipt: ReceiptIdentity,
        budget: SharedBudget,
        holder: HolderId,
        token: TokenId,
        coordinator: Arc<SettlementCoordinator>,
    }
    impl Fixture {
        fn new() -> Self {
            Self::with_clock(Arc::new(ManualClock::new(UnixTsMillis(123))), 30_000)
        }
        fn with_clock(clock: Arc<dyn ardur_cost_gate::Clock>, ttl: u64) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let parent = temp.path().canonicalize().unwrap();
            let root = parent.join("store");
            let receipt = ReceiptIdentity {
                receipt_log: parent.join("receipts.jsonl"),
                signer: Sha256Digest::of(b"owner spec fixture"),
            };
            let budget = SharedBudget::new();
            let holder = HolderId("holder".into());
            let token = TokenId(Uuid::new_v4());
            budget.set_balance(holder.clone(), CostTuple::cents(30));
            let gate = Arc::new(
                InMemoryCostAdmissionGate::with_clock(budget.clone(), clock).with_ttl_ms(ttl),
            );
            gate.bind_token(token, holder.clone());
            let coordinator = SettlementCoordinator::open(gate, &root, receipt.clone()).unwrap();
            Self {
                _temp: temp,
                root,
                receipt,
                budget,
                holder,
                token,
                coordinator,
            }
        }
        fn attribution(&self, journal: bool) -> TurnIdentity {
            TurnIdentity {
                request_session: SessionId(Uuid::new_v4()),
                journal_owner: journal.then(|| SessionId(Uuid::new_v4())),
                verified_subject: HolderId("subject".into()),
                budget_holder: self.holder.clone(),
                cap_token_id: self.token,
                started_at: UnixTsMillis(123),
            }
        }
        fn request(&self) -> AdmissionRequest {
            AdmissionRequest {
                cap_token_id: self.token,
                projected_envelope: CostEnvelope {
                    cents_max: 10,
                    ..Default::default()
                },
                provider_id: ProviderId("provider".into()),
                model_id: ModelId("model".into()),
                request_digest: Sha256Digest::of(b"request"),
            }
        }
        async fn owner(&self, journal: bool) -> TurnSettlementOwner {
            let mut owner = self
                .coordinator
                .reserve_turn(self.attribution(journal))
                .unwrap();
            owner
                .admit_round(self.request(), Uuid::new_v4())
                .await
                .unwrap();
            owner
        }
        fn saved(&self, id: TurnId) -> EncodedSnapshot {
            load_settlement_snapshot(&self.root, &self.receipt, &LIMITS)
                .unwrap()
                .snapshots
                .into_iter()
                .find(|s| s.turn().turn_id == id)
                .unwrap()
        }
    }
    // Lower-level capacity invariants, not additional public historical exploits.
    #[tokio::test]
    async fn already_reserved_owner_rechecks_ordinal_capacity_before_hold() {
        let f = Fixture::new();
        let mut owner = f.coordinator.reserve_turn(f.attribution(false)).unwrap();
        f.coordinator.state.lock().next_ordinal = u64::MAX;
        let result = owner.admit_round(f.request(), Uuid::new_v4()).await;
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(30),
            "reserved owner must recheck capacity before economic admission"
        );
        assert!(matches!(
            result,
            Err(SettlementError::Invalid(SettlementProblem::Bounds))
        ));
        let status = f.coordinator.status();
        assert!(status.turns[0].admission_attempt.is_none());
        assert!(status.turns[0].view.is_none());
        assert!(status.turns[0].turn.rounds.is_empty());
        assert!(
            load_settlement_snapshot(&f.root, &f.receipt, &LIMITS)
                .unwrap()
                .snapshots
                .is_empty()
        );
        owner.close_empty().unwrap();
        assert!(f.coordinator.status().turns.is_empty());
    }
    #[tokio::test]
    async fn admitted_owner_reserves_future_cancellation_ordinal() {
        let f = Fixture::new();
        let mut owner = f.owner(true).await;
        let id = owner.turn_id();
        // Simulate exhaustion elsewhere AFTER admission; its promised capacity
        // must not be consumed again at cancellation/Drop.
        f.coordinator.state.lock().next_ordinal = u64::MAX;
        owner
            .abandon_sync()
            .expect("admission must reserve future cancellation capacity");
        let first = f.saved(id);
        assert_eq!(first.turn().rounds[0].commit_ordinal, Some(0));
        assert!(matches!(
            first.turn().rounds[0].projection,
            JournalProjection::Pending(_)
        ));
        owner.abandon_sync().unwrap();
        drop(owner);
        assert_eq!(f.saved(id).bytes(), first.bytes());
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(30)
        );
        assert_eq!(f.coordinator.state.lock().next_ordinal, u64::MAX);
    }
    // Lower-level defensive probes reconstruct stale local facts with real
    // acknowledged disk history. Early admission refusal makes the original
    // public exhausted-history path unreachable; these are not new exploits.
    async fn stale_prepared_case(previously_prepared: bool) {
        let clock = Arc::new(AdvancingClock::default());
        let f = Fixture::with_clock(clock.clone(), 30_000);
        let mut owner = f.owner(false).await;
        owner.observe_provider(known9()).unwrap();
        let id = owner.turn_id();
        if previously_prepared {
            owner
                .prepare(
                    Disposition::Refusal(RefusalClass::Authorization),
                    CostTuple::cents(9),
                )
                .unwrap();
        }
        let disk = f.saved(id);
        {
            let mut state = f.coordinator.state.lock();
            let slot = state.slots.get_mut(&id).unwrap().as_mut().unwrap();
            assert_eq!(slot.acknowledged.as_ref().unwrap().bytes(), disk.bytes());
            assert!(slot.error.is_none() && slot.pending.is_none() && !slot.dirty);
            if previously_prepared {
                slot.turn.started_at = UnixTsMillis(456);
            } else {
                slot.turn.rounds.last_mut().expect("current round").phase =
                    SettlementPhase::Prepared { candidate: None };
                slot.turn.rounds.last_mut().expect("current round").decision =
                    Some(SettlementDecision::Refusal(RefusalClass::Authorization));
                slot.requested = Some(CostTuple::cents(9));
            }
        }
        let before = clock.0.load(std::sync::atomic::Ordering::SeqCst);
        let result = owner.finalize_sync();
        assert_eq!(
            clock.0.load(std::sync::atomic::Ordering::SeqCst),
            before,
            "finalization must not enter the gate without EXACT acknowledged current Prepared facts"
        );
        assert!(matches!(result, Err(SettlementError::AdmissionClosed)));
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(20)
        );
        assert_eq!(f.saved(id).bytes(), disk.bytes());
        let status = f.coordinator.status();
        assert!(status.turns[0].undurable);
        assert!(matches!(
            status.turns[0].error,
            Some(SettlementError::Invalid(
                SettlementProblem::InvalidTransition
            ))
        ));
        assert_eq!(
            status.turns[0].view.as_ref().unwrap().status,
            OwnedBudgetStatus::Active
        );
        assert!(status.turns[0].view.as_ref().unwrap().attempt.is_none());
        drop(owner);
        assert!(matches!(
            f.coordinator.reserve_turn(f.attribution(false)),
            Err(SettlementError::AdmissionClosed)
        ));
    }
    #[tokio::test]
    async fn stale_local_prepared_cannot_finalize_over_durable_work_observed() {
        stale_prepared_case(false).await;
    }
    #[tokio::test]
    async fn stale_local_prepared_requires_exact_not_merely_prepared_ack() {
        stale_prepared_case(true).await;
    }
    fn install_stale_cancelled(f: &Fixture, id: TurnId) {
        let mut state = f.coordinator.state.lock();
        let slot = state.slots.get_mut(&id).unwrap().as_mut().unwrap();
        assert!(slot.error.is_none() && !slot.dirty && slot.pending.is_none());
        slot.turn.rounds.last_mut().expect("current round").decision =
            Some(SettlementDecision::Cancelled);
        slot.turn.rounds.last_mut().expect("current round").phase =
            SettlementPhase::Prepared { candidate: None };
        slot.turn.terminal = TurnTerminal::Cancelled;
    }
    #[tokio::test]
    async fn stale_cancelled_repeat_cannot_claim_release_without_real_credit() {
        let f = Fixture::new();
        let mut owner = f.owner(false).await;
        let id = owner.turn_id();
        let disk = f.saved(id);
        install_stale_cancelled(&f, id);
        let result = owner.abandon_sync();
        assert!(
            matches!(result, Err(SettlementError::AdmissionClosed)),
            "a local Cancelled decision is not release closure"
        );
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(20)
        );
        assert_eq!(f.saved(id).bytes(), disk.bytes());
        let status = f.coordinator.status();
        assert_eq!(
            status.turns[0].view.as_ref().unwrap().status,
            OwnedBudgetStatus::Active
        );
        assert!(status.turns[0].view.as_ref().unwrap().refund.is_none());
        assert!(status.turns[0].undurable);
        assert!(matches!(
            status.turns[0].error,
            Some(SettlementError::Invalid(
                SettlementProblem::InvalidTransition
            ))
        ));
    }
    #[tokio::test]
    async fn stale_cancelled_drop_retains_abandonment_without_release_continuation() {
        let f = Fixture::new();
        let owner = f.owner(false).await;
        let id = owner.turn_id();
        let disk = f.saved(id);
        install_stale_cancelled(&f, id);
        drop(owner);
        assert!(
            f.coordinator.status().turns[0].abandonment_pending,
            "decision alone cannot discharge owning Drop abandonment"
        );
        assert!(matches!(
            f.coordinator.supervisor().retry_abandonment(id),
            Err(SettlementError::AdmissionClosed)
        ));
        assert!(matches!(
            f.coordinator.reserve_turn(f.attribution(false)),
            Err(SettlementError::AdmissionClosed)
        ));
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(20)
        );
        assert_eq!(f.saved(id).bytes(), disk.bytes());
        assert!(f.coordinator.supervisor().try_close().is_err());
    }
    #[tokio::test]
    async fn missing_reserved_ordinal_leaves_selection_atomic_and_quarantined() {
        // Private capacity-corruption invariant, not a public retained-history
        // reproduction: real admission now reserves this ordinal in advance.
        for cancel in [false, true] {
            let f = Fixture::new();
            let mut owner = f.owner(false).await;
            owner.observe_provider(known9()).unwrap();
            let id = owner.turn_id();
            let disk = f.saved(id);
            f.coordinator
                .state
                .lock()
                .slots
                .get_mut(&id)
                .unwrap()
                .as_mut()
                .unwrap()
                .projection_ordinal = None;
            let result = if cancel {
                owner.abandon_sync()
            } else {
                owner.prepare(
                    Disposition::Refusal(RefusalClass::Authorization),
                    CostTuple::cents(9),
                )
            };
            assert!(matches!(
                result,
                Err(SettlementError::Invalid(SettlementProblem::Bounds))
            ));
            let status = f.coordinator.status();
            assert_eq!(&status.turns[0].turn, disk.turn());
            assert!(!status.turns[0].undurable);
            assert!(matches!(
                status.turns[0].error,
                Some(SettlementError::Invalid(SettlementProblem::Bounds))
            ));
            assert!(
                f.coordinator.state.lock().slots[&id]
                    .as_ref()
                    .unwrap()
                    .requested
                    .is_none()
            );
            assert!(matches!(
                owner.finalize_sync(),
                Err(SettlementError::AdmissionClosed)
            ));
            assert!(matches!(
                owner.abandon_sync(),
                Err(SettlementError::AdmissionClosed)
            ));
            drop(owner);
            assert!(f.coordinator.status().turns[0].abandonment_pending);
            assert_eq!(f.saved(id).bytes(), disk.bytes());
            assert_eq!(
                f.budget.current_balance(&f.holder).await.unwrap(),
                CostTuple::cents(20)
            );
            assert!(matches!(
                f.coordinator.reserve_turn(f.attribution(false)),
                Err(SettlementError::AdmissionClosed)
            ));
        }
    }
    #[tokio::test]
    async fn exact_prepared_ack_enters_gate_once_and_keeps_paid_disposition() {
        let clock = Arc::new(AdvancingClock::default());
        let f = Fixture::with_clock(clock.clone(), 30_000);
        let mut owner = f.owner(false).await;
        let id = owner.turn_id();
        owner.observe_provider(known9()).unwrap();
        owner
            .prepare(
                Disposition::Refusal(RefusalClass::Authorization),
                CostTuple::cents(9),
            )
            .unwrap();
        let prepared = f.saved(id);
        assert_eq!(prepared.turn(), &f.coordinator.status().turns[0].turn);
        let calls = clock.0.load(std::sync::atomic::Ordering::SeqCst);
        owner.finalize_sync().unwrap();
        assert_eq!(clock.0.load(std::sync::atomic::Ordering::SeqCst), calls + 1);
        assert!(matches!(owner.finalize_sync(), Err(SettlementError::State)));
        assert_eq!(clock.0.load(std::sync::atomic::Ordering::SeqCst), calls + 1);
        owner.commit_disposition().unwrap();
        drop(owner);
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(21)
        );
        assert!(f.coordinator.supervisor().try_close().is_ok());
    }
    #[tokio::test]
    async fn concurrent_new_abandonment_waits_for_public_retry_after_bounded_drain() {
        use std::sync::mpsc;
        use std::time::Duration;
        struct ResumeOnDrop(Option<mpsc::Sender<()>>);
        impl Drop for ResumeOnDrop {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }
        let f = Fixture::new();
        let a = f.owner(true).await;
        let aid = a.turn_id();
        drop(a);
        let a_disk = f.saved(aid);
        assert!(matches!(
            a_disk.turn().rounds[0].projection,
            JournalProjection::Pending(_)
        ));
        let mut b = f.owner(true).await;
        b.observe_provider(known9()).unwrap();
        let bid = b.turn_id();
        let b_disk = f.saved(bid);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        *f.coordinator.drain_pause.lock() = Some(DrainPause {
            entered: entered_tx,
            resume: resume_rx,
        });
        let release = ResumeOnDrop(Some(resume_tx));
        let supervisor = f.coordinator.supervisor();
        // The public retry performs the real snapshot; no fake Busy/draining bits,
        // duplicate owner, sleep, recursive pass or unbounded thread join.
        let worker = std::thread::spawn(move || {
            let result = supervisor.retry_abandonment(aid);
            let _ = done_tx.send(result);
        });
        let ids = entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!ids.contains(&bid));
        assert!(f.coordinator.state.lock().draining);
        drop(b);
        let during = f.coordinator.status();
        assert!(
            during
                .turns
                .iter()
                .find(|s| s.turn.turn_id == bid)
                .unwrap()
                .abandonment_pending
        );
        assert!(matches!(
            f.coordinator.reserve_turn(f.attribution(false)),
            Err(SettlementError::AdmissionClosed)
        ));
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(20)
        );
        assert_eq!(f.saved(bid).bytes(), b_disk.bytes());
        drop(release);
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        // Result delivery proves retry returned. Do not join an unbounded worker.
        drop(worker);
        assert!(!f.coordinator.state.lock().draining);
        assert!(
            f.coordinator
                .status()
                .turns
                .iter()
                .find(|s| s.turn.turn_id == bid)
                .unwrap()
                .abandonment_pending
        );
        assert_eq!(f.saved(bid).bytes(), b_disk.bytes());
        assert!(matches!(
            f.coordinator.reserve_turn(f.attribution(false)),
            Err(SettlementError::AdmissionClosed)
        ));
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(20)
        );
        let supervisor = f.coordinator.supervisor();
        supervisor.retry_abandonment(bid).unwrap();
        assert!(
            !supervisor
                .status()
                .turns
                .iter()
                .find(|s| s.turn.turn_id == bid)
                .unwrap()
                .abandonment_pending
        );
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(30)
        );
        let cancelled = f.saved(bid);
        assert_eq!(cancelled.turn().terminal, TurnTerminal::Cancelled);
        assert_eq!(
            cancelled.turn().rounds[0].known_incurred,
            CostTuple::cents(9)
        );
        assert_eq!(cancelled.turn().rounds[0].commit_ordinal, Some(1));
        supervisor.retry_abandonment(bid).unwrap();
        assert_eq!(f.saved(bid).bytes(), cancelled.bytes());
        assert_eq!(f.saved(aid).bytes(), a_disk.bytes());
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(30)
        );
        f.coordinator
            .reserve_turn(f.attribution(false))
            .unwrap()
            .close_empty()
            .unwrap();
    }
    #[derive(Default)]
    struct AdvancingClock(std::sync::atomic::AtomicU64);
    impl ardur_cost_gate::Clock for AdvancingClock {
        fn now_ms(&self) -> UnixTsMillis {
            UnixTsMillis(self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
        }
    }
    #[tokio::test]
    async fn expired_claim_retains_first_hold_and_latches_admission() {
        let f = Fixture::with_clock(Arc::new(AdvancingClock::default()), 0);
        let mut owner = f.coordinator.reserve_turn(f.attribution(false)).unwrap();
        let id = owner.turn_id();
        let provider = Uuid::new_v4();
        let first = owner.admit_round(f.request(), provider).await;
        assert!(
            matches!(first, Err(SettlementError::Budget(ref e)) if matches!(e.as_ref(), AdmissionError::ReservationExpired))
        );
        let first_id = f.coordinator.status().turns[0]
            .unclaimed_reservation
            .unwrap();
        let before = f.budget.current_balance(&f.holder).await.unwrap();
        assert_eq!(before, CostTuple::cents(20));
        let second = owner.admit_round(f.request(), Uuid::new_v4()).await;
        let after = f.budget.current_balance(&f.holder).await.unwrap();
        let after_id = f.coordinator.status().turns[0].unclaimed_reservation;
        assert_eq!(
            after, before,
            "failed claim must not permit a second economic admission"
        );
        assert_eq!(after_id, Some(first_id));
        let retained = f.coordinator.status().turns[0]
            .admission_attempt
            .clone()
            .unwrap();
        assert_eq!(retained.request.cap_token_id, f.token);
        assert_eq!(
            retained.request.projected_envelope,
            f.request().projected_envelope
        );
        assert_eq!(retained.request.provider_id, f.request().provider_id);
        assert_eq!(retained.request.model_id, f.request().model_id);
        assert_eq!(retained.request.request_digest, f.request().request_digest);
        assert_eq!(retained.provider_request_id, provider);
        {
            let state = f.coordinator.state.lock();
            let returned = state.slots[&id]
                .as_ref()
                .unwrap()
                .unclaimed
                .as_ref()
                .unwrap();
            assert_eq!(returned.reservation_id, first_id);
            assert_eq!(returned.cap_token_id, f.token);
            assert_eq!(returned.envelope, f.request().projected_envelope);
            assert_eq!(returned.reserved_at, returned.expires_at);
        }
        assert!(matches!(
            second,
            Err(SettlementError::State | SettlementError::AdmissionClosed)
        ));
        assert!(matches!(
            f.coordinator.status().turns[0].error,
            Some(SettlementError::Budget(_))
        ));
        drop(owner);
        assert!(matches!(
            f.coordinator.reserve_turn(f.attribution(false)),
            Err(SettlementError::AdmissionClosed)
        ));
        assert_eq!(f.coordinator.status().turns[0].turn.turn_id, id);
        assert_eq!(
            f.coordinator.status().turns[0].unclaimed_reservation,
            Some(first_id)
        );
        assert!(f.coordinator.supervisor().try_close().is_err());
    }
    async fn settled_ack_case(drop_before_ack: bool, journal: bool) {
        let f = Fixture::new();
        let mut owner = f.owner(journal).await;
        let id = owner.turn_id();
        owner.observe_provider(known9()).unwrap();
        owner
            .prepare(
                Disposition::Refusal(RefusalClass::Authorization),
                CostTuple::cents(9),
            )
            .unwrap();
        owner.finalize_sync().unwrap();
        let before = f.saved(id);
        let hold = before.turn().rounds[0].settlement_id.0;
        let balance = f.budget.current_balance(&f.holder).await.unwrap();
        assert_eq!(balance, CostTuple::cents(21));
        *f.coordinator.ack_delivery_fault.lock() =
            Some((id, before.revision() + 1, f.root.clone()));
        assert!(matches!(
            owner.commit_disposition(),
            Err(SettlementError::Storage(
                SettlementProblem::VerificationFailed
            ))
        ));
        assert!(f.coordinator.ack_delivery_fault.lock().is_none());
        let status = f.coordinator.status();
        let status = &status.turns[0];
        let durable = f.saved(id);
        assert_ne!(
            durable.bytes(),
            before.bytes(),
            "a no-op write cannot satisfy durable Settled"
        );
        assert_ne!(durable.digest(), before.digest());
        assert_eq!(durable.revision(), before.revision() + 1);
        assert_eq!(status.pending_revision, Some(durable.revision()));
        assert_eq!(status.pending_digest, Some(durable.digest()));
        assert_eq!(status.acknowledged_revision, Some(before.revision()));
        assert_eq!(
            status.view.as_ref().unwrap().status,
            OwnedBudgetStatus::Finalized
        );
        assert!(status.retirement_pending);
        assert!(matches!(
            durable.turn().rounds[0].phase,
            SettlementPhase::Settled { .. }
        ));
        assert_eq!(f.coordinator.store.lock().health(), StoreHealth::Healthy);
        {
            let state = f.coordinator.state.lock();
            let pending = state.slots[&id].as_ref().unwrap().pending.as_ref().unwrap();
            assert_eq!(pending.previous, Some(before.revision()));
            assert_eq!(pending.encoded.bytes(), durable.bytes());
        }
        let supervisor = f.coordinator.supervisor();
        let mut owner = Some(owner);
        if drop_before_ack {
            drop(owner.take());
        }
        supervisor.resolve_storage(id).unwrap();
        assert!(
            f.coordinator.gate.pending_owned(hold).is_none(),
            "exact Settled acknowledgement must retire the existing Finalized capability"
        );
        if let Some(owner) = owner.as_mut() {
            assert_eq!(
                supervisor.status().turns[0].view.as_ref().unwrap().status,
                OwnedBudgetStatus::Committed
            );
            owner.commit_disposition().unwrap();
        }
        assert!(matches!(
            supervisor.resolve_storage(id),
            Err(SettlementError::State)
        ));
        assert_eq!(
            f.saved(id).bytes(),
            durable.bytes(),
            "retirement must not mint another revision"
        );
        assert_eq!(f.budget.current_balance(&f.holder).await.unwrap(), balance);
        drop(owner);
        if journal {
            let retained = supervisor.status();
            assert_eq!(retained.turns.len(), 1);
            assert_eq!(
                retained.turns[0].view.as_ref().unwrap().status,
                OwnedBudgetStatus::Committed
            );
            assert!(matches!(
                retained.turns[0].turn.rounds[0].projection,
                JournalProjection::Pending(_)
            ));
            assert!(supervisor.try_close().is_err());
        } else {
            assert!(supervisor.status().turns.is_empty());
            assert!(supervisor.try_close().is_ok());
        }
    }
    #[tokio::test]
    async fn settled_ack_retires_retained_owner_without_recharge() {
        settled_ack_case(false, false).await;
    }
    #[tokio::test]
    async fn settled_ack_retires_dropped_owner_and_allows_closure() {
        settled_ack_case(true, false).await;
    }
    #[tokio::test]
    async fn settled_ack_retires_authority_but_keeps_projection_backlog() {
        settled_ack_case(true, true).await;
    }
    async fn initial_ack_mismatch_case(fault: bool) {
        let f = Fixture::new();
        let expected = HolderId("expected-holder-A".into());
        f.budget.set_balance(expected.clone(), CostTuple::cents(30));
        let mut identity = f.attribution(false);
        identity.budget_holder = expected.clone();
        let mut owner = f.coordinator.reserve_turn(identity).unwrap();
        let id = owner.turn_id();
        assert!(
            !f.root.join(format!("{}.json", id.0)).exists(),
            "no-op negative control: no Reserved file exists yet"
        );
        if fault {
            *f.coordinator.ack_delivery_fault.lock() = Some((id, 1, f.root.clone()));
        }
        let admission = owner.admit_round(f.request(), Uuid::new_v4()).await;
        if fault {
            assert!(matches!(
                admission,
                Err(SettlementError::Storage(
                    SettlementProblem::VerificationFailed
                ))
            ));
        } else {
            assert!(matches!(admission, Err(SettlementError::IdentityMismatch)));
        }
        let disk = f.saved(id);
        let status = f.coordinator.status();
        assert_eq!(status.turns[0].expected_holder, expected);
        assert!(matches!(
            status.turns[0].validation_error,
            Some(SettlementError::IdentityMismatch)
        ));
        if fault {
            assert!(matches!(
                status.turns[0].error,
                Some(SettlementError::Storage(_))
            ));
        }
        assert_eq!(disk.revision(), 1);
        assert_eq!(disk.turn().budget_holder, f.holder);
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(20)
        );
        assert_eq!(
            f.budget.current_balance(&expected).await.unwrap(),
            CostTuple::cents(30)
        );
        let supervisor = f.coordinator.supervisor();
        if fault {
            assert_eq!(
                supervisor.status().turns[0].pending_digest,
                Some(disk.digest())
            );
            let resolved = supervisor.resolve_storage(id);
            assert!(
                matches!(
                    supervisor.status().turns[0].error,
                    Some(SettlementError::IdentityMismatch)
                ),
                "storage acknowledgement must not erase the independently rejected actual holder"
            );
            assert!(matches!(resolved, Err(SettlementError::IdentityMismatch)));
            assert_eq!(f.saved(id).bytes(), disk.bytes());
        }
        assert!(matches!(
            owner.observe_provider(known9()),
            Err(SettlementError::AdmissionClosed)
        ));
        assert!(matches!(
            owner.prepare(
                Disposition::Refusal(RefusalClass::Authorization),
                CostTuple::ZERO
            ),
            Err(SettlementError::AdmissionClosed)
        ));
        assert!(
            owner
                .admit_round(f.request(), Uuid::new_v4())
                .await
                .is_err()
        );
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(20)
        );
        assert_eq!(
            supervisor.status().turns[0].view.as_ref().unwrap().status,
            OwnedBudgetStatus::Active
        );
        drop(owner);
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(30)
        );
        assert_eq!(f.saved(id).turn().terminal, TurnTerminal::Cancelled);
        assert!(matches!(
            supervisor.status().turns[0].error,
            Some(SettlementError::IdentityMismatch)
        ));
        assert!(matches!(
            f.coordinator.reserve_turn(f.attribution(false)),
            Err(SettlementError::AdmissionClosed)
        ));
    }
    #[tokio::test]
    async fn initial_ack_recovery_cannot_validate_wrong_holder() {
        initial_ack_mismatch_case(true).await;
    }
    #[tokio::test]
    async fn initial_ack_no_fault_mismatch_stays_invalid() {
        initial_ack_mismatch_case(false).await;
    }
    #[tokio::test]
    async fn initial_ack_valid_holder_recovery_allows_work() {
        let f = Fixture::new();
        let mut owner = f.coordinator.reserve_turn(f.attribution(false)).unwrap();
        let id = owner.turn_id();
        *f.coordinator.ack_delivery_fault.lock() = Some((id, 1, f.root.clone()));
        assert!(matches!(
            owner.admit_round(f.request(), Uuid::new_v4()).await,
            Err(SettlementError::Storage(_))
        ));
        let disk = f.saved(id);
        assert_eq!(disk.revision(), 1);
        f.coordinator.supervisor().resolve_storage(id).unwrap();
        assert_eq!(f.saved(id).bytes(), disk.bytes());
        owner.observe_provider(known9()).unwrap();
        owner
            .prepare(
                Disposition::Refusal(RefusalClass::Authorization),
                CostTuple::cents(9),
            )
            .unwrap();
        owner.finalize_sync().unwrap();
        owner.commit_disposition().unwrap();
        drop(owner);
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(21)
        );
        assert!(f.coordinator.supervisor().try_close().is_ok());
    }
    #[tokio::test]
    async fn recovered_reserved_ack_keeps_independent_pending_credit() {
        let f = Fixture::new();
        let mut owner = f.coordinator.reserve_turn(f.attribution(false)).unwrap();
        let id = owner.turn_id();
        *f.coordinator.ack_delivery_fault.lock() = Some((id, 1, f.root.clone()));
        assert!(matches!(
            owner.admit_round(f.request(), Uuid::new_v4()).await,
            Err(SettlementError::Storage(_))
        ));
        f.budget
            .set_balance(f.holder.clone(), CostTuple::cents(u64::MAX - 3));
        assert!(matches!(
            owner.abandon_sync(),
            Err(SettlementError::Storage(_))
        ));
        let supervisor = f.coordinator.supervisor();
        let before = supervisor.status();
        assert_eq!(
            before.turns[0].view.as_ref().unwrap().status,
            OwnedBudgetStatus::ReleasePending
        );
        assert_eq!(
            before.turns[0]
                .view
                .as_ref()
                .unwrap()
                .refund
                .as_ref()
                .unwrap()
                .remaining_credit,
            CostTuple::cents(7)
        );
        assert!(matches!(
            supervisor.resolve_storage(id),
            Err(SettlementError::CreditPending(
                SettlementProblem::ReleaseCreditPending
            ))
        ));
        assert!(matches!(
            supervisor.status().turns[0].error,
            Some(SettlementError::CreditPending(_))
        ));
        assert!(!supervisor.status().turns[0].retirement_pending);
        assert!(owner.commit_disposition().is_err());
        drop(owner);
        f.budget
            .try_reserve(
                &f.holder,
                &CostEnvelope {
                    cents_max: 7,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        supervisor.retry_refund(id).unwrap();
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(u64::MAX)
        );
        assert!(supervisor.try_close().is_ok());
    }
    #[tokio::test]
    async fn dropped_invalid_owner_recovery_retains_validation_after_release() {
        let f = Fixture::new();
        let mut identity = f.attribution(false);
        identity.budget_holder = HolderId("expected-other".into());
        let mut owner = f.coordinator.reserve_turn(identity).unwrap();
        let id = owner.turn_id();
        *f.coordinator.ack_delivery_fault.lock() = Some((id, 1, f.root.clone()));
        assert!(matches!(
            owner.admit_round(f.request(), Uuid::new_v4()).await,
            Err(SettlementError::Storage(_))
        ));
        let original = f.saved(id);
        drop(owner);
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(30)
        );
        assert_eq!(f.saved(id).bytes(), original.bytes());
        let supervisor = f.coordinator.supervisor();
        assert!(matches!(
            supervisor.status().turns[0].validation_error,
            Some(SettlementError::IdentityMismatch)
        ));
        assert!(matches!(
            supervisor.resolve_storage(id),
            Err(SettlementError::IdentityMismatch)
        ));
        assert_eq!(
            supervisor.status().turns[0].view.as_ref().unwrap().status,
            OwnedBudgetStatus::Released
        );
        assert!(matches!(
            supervisor.status().turns[0].error,
            Some(SettlementError::IdentityMismatch)
        ));
        assert_eq!(f.saved(id).turn().terminal, TurnTerminal::Cancelled);
        assert!(supervisor.try_close().is_err());
    }
    #[tokio::test]
    async fn refused_admission_closes_unused_capacity_without_reusing_ordinal() {
        let f = Fixture::new();
        f.budget.set_balance(f.holder.clone(), CostTuple::cents(5));
        let mut owner = f.coordinator.reserve_turn(f.attribution(false)).unwrap();
        let refused_id = owner.turn_id();
        assert!(matches!(
            owner.admit_round(f.request(), Uuid::new_v4()).await,
            Err(SettlementError::Budget(ref e))
                if matches!(e.as_ref(), AdmissionError::BudgetExhausted { .. })
        ));
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(5)
        );
        let status = f.coordinator.status();
        assert!(status.turns[0].admission_attempt.is_none());
        assert!(status.turns[0].unclaimed_reservation.is_none());
        assert!(status.turns[0].error.is_none());
        owner.close_empty().unwrap();
        assert!(f.coordinator.status().turns.is_empty());
        assert_eq!(f.coordinator.status().inventory_count, 0);
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(5)
        );
        assert!(
            load_settlement_snapshot(&f.root, &f.receipt, &LIMITS)
                .unwrap()
                .snapshots
                .is_empty()
        );

        f.budget.set_balance(f.holder.clone(), CostTuple::cents(30));
        let mut funded = f.owner(false).await;
        assert_ne!(funded.turn_id(), refused_id);
        funded.abandon_sync().unwrap();
        assert_eq!(
            f.saved(funded.turn_id()).turn().rounds[0].commit_ordinal,
            Some(1)
        );
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(30)
        );
        drop(funded);
        assert!(f.coordinator.supervisor().try_close().is_ok());
    }

    struct AdmissionPanicClock;
    impl ardur_cost_gate::Clock for AdmissionPanicClock {
        fn now_ms(&self) -> UnixTsMillis {
            panic!("ordinary admission clock after real reserve");
        }
    }
    #[test]
    fn admission_panic_cannot_close_uncertain_empty_slot() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let f = Fixture::with_clock(Arc::new(AdmissionPanicClock), 30_000);
        f.budget.set_balance(f.holder.clone(), CostTuple::cents(10));
        let mut owner = f.coordinator.reserve_turn(f.attribution(false)).unwrap();
        let id = owner.turn_id();
        let supervisor = f.coordinator.supervisor();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                rt.block_on(owner.admit_round(f.request(), Uuid::new_v4()))
            }))
            .is_err()
        );
        assert_eq!(
            rt.block_on(f.budget.current_balance(&f.holder)).unwrap(),
            CostTuple::ZERO
        );
        assert!(matches!(
            supervisor.status().turns[0].error,
            Some(SettlementError::Panicked)
        ));
        let close = owner.close_empty();
        assert!(
            close.is_err(),
            "uncertain economic admission cannot become successful empty closure"
        );
        let supervisor = supervisor
            .try_close()
            .expect_err("uncertain capacity must stay externally owned");
        assert_eq!(supervisor.status().turns[0].turn.turn_id, id);
        assert!(matches!(
            supervisor.status().turns[0].error,
            Some(SettlementError::Panicked)
        ));
        assert_eq!(supervisor.status().inventory_count, 1);
        assert!(matches!(
            f.coordinator.reserve_turn(f.attribution(false)),
            Err(SettlementError::AdmissionClosed)
        ));
        assert!(
            load_settlement_snapshot(&f.root, &f.receipt, &LIMITS)
                .unwrap()
                .snapshots
                .is_empty()
        );
    }
    #[test]
    fn untouched_capacity_closes_without_economic_record() {
        let f = Fixture::new();
        f.coordinator
            .reserve_turn(f.attribution(false))
            .unwrap()
            .close_empty()
            .unwrap();
        assert!(f.coordinator.status().turns.is_empty());
        assert_eq!(f.coordinator.status().inventory_count, 0);
        assert!(
            load_settlement_snapshot(&f.root, &f.receipt, &LIMITS)
                .unwrap()
                .snapshots
                .is_empty()
        );
        assert!(f.coordinator.supervisor().try_close().is_ok());
    }
    fn known9() -> ProviderEvidence {
        ProviderEvidence::Observed {
            usage: None,
            cost: CostTuple::cents(9),
            provenance: CostProvenance::ReportedCost,
            finished: false,
            interrupted: false,
        }
    }
    #[tokio::test]
    async fn busy_same_slot_handback_finishes_owner_abandonment() {
        let f = Fixture::new();
        let mut owner = f.owner(false).await;
        owner.observe_provider(known9()).unwrap();
        let id = owner.turn_id();
        let held = f.coordinator.take(id).unwrap();
        drop(owner);
        assert!(f.coordinator.status().turns[0].abandonment_pending);
        drop(held);
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(30)
        );
        assert_eq!(f.saved(id).turn().terminal, TurnTerminal::Cancelled);
        assert!(f.coordinator.supervisor().try_close().is_ok());
    }
    #[tokio::test]
    async fn busy_handback_preserves_prepared_and_finalized_decisions() {
        for finalized in [false, true] {
            let f = Fixture::new();
            let mut owner = f.owner(false).await;
            owner.observe_provider(known9()).unwrap();
            owner
                .prepare(
                    Disposition::Refusal(RefusalClass::Authorization),
                    CostTuple::cents(9),
                )
                .unwrap();
            if finalized {
                owner.finalize_sync().unwrap();
            }
            let id = owner.turn_id();
            let before = f.saved(id);
            let balance = f.budget.current_balance(&f.holder).await.unwrap();
            let held = f.coordinator.take(id).unwrap();
            drop(owner);
            drop(held);
            assert_eq!(f.saved(id).bytes(), before.bytes());
            assert_eq!(f.budget.current_balance(&f.holder).await.unwrap(), balance);
            let status = f.coordinator.status();
            assert!(!status.turns[0].abandonment_pending);
            assert_eq!(
                status.turns[0].view.as_ref().unwrap().status,
                if finalized {
                    OwnedBudgetStatus::Finalized
                } else {
                    OwnedBudgetStatus::Active
                }
            );
            assert!(f.coordinator.supervisor().try_close().is_err());
        }
    }
    #[tokio::test]
    async fn unwinding_handback_retains_public_abandonment_continuation() {
        let f = Fixture::new();
        let mut owner = f.owner(false).await;
        owner.observe_provider(known9()).unwrap();
        let id = owner.turn_id();
        let supervisor = f.coordinator.supervisor();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _held = f.coordinator.take(id).unwrap();
                drop(owner);
                panic!("controlled handback unwind");
            }))
            .is_err()
        );
        assert!(supervisor.status().turns[0].abandonment_pending);
        assert!(matches!(
            f.coordinator.reserve_turn(f.attribution(false)),
            Err(SettlementError::AdmissionClosed)
        ));
        // The panic error remains independent even after the cancellation credit.
        assert!(matches!(
            supervisor.retry_abandonment(id),
            Err(SettlementError::Panicked)
        ));
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(30)
        );
        assert_eq!(f.saved(id).turn().terminal, TurnTerminal::Cancelled);
        assert!(!supervisor.status().turns[0].abandonment_pending);
    }
    #[tokio::test]
    async fn busy_unrelated_handback_finishes_owner_abandonment() {
        let f = Fixture::new();
        let a = f.owner(true).await;
        let aid = a.turn_id();
        drop(a);
        assert!(matches!(
            f.saved(aid).turn().rounds[0].projection,
            JournalProjection::Pending(_)
        ));
        let mut b = f.owner(true).await;
        let bid = b.turn_id();
        b.observe_provider(known9()).unwrap();
        let before = f.saved(bid);
        assert_eq!(
            f.budget.current_balance(&f.holder).await.unwrap(),
            CostTuple::cents(20)
        );
        // Exact public resolve_storage(A) window: a REAL moved slot/authority
        // handback, paused before checking A.pending. No fake Busy flag.
        let held = f.coordinator.take(aid).unwrap();
        assert!(held.slot.as_ref().unwrap().pending.is_none());
        drop(b);
        assert_eq!(f.coordinator.status().busy, Some(aid));
        drop(held);
        let after = f.saved(bid);
        let balance = f.budget.current_balance(&f.holder).await.unwrap();
        assert_eq!(
            balance,
            CostTuple::cents(30),
            "Busy handback must finish retained abandonment and fully release B"
        );
        assert_ne!(after.digest(), before.digest());
        assert_eq!(after.turn().terminal, TurnTerminal::Cancelled);
        let round = &after.turn().rounds[0];
        assert_eq!(round.known_incurred, CostTuple::cents(9));
        let SettlementPhase::Settled {
            application,
            receipt: None,
        } = &round.phase
        else {
            panic!("durable cancelled closure required")
        };
        assert_eq!(application.requested_debit, CostTuple::ZERO);
        assert_eq!(application.applied_debit, CostTuple::ZERO);
        assert_eq!(application.reserved_credit, CostTuple::cents(10));
        assert_eq!(f.coordinator.status().executing, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_and_scanner_errors_project_their_own_reason_strings() {
        // #543: an operational approval failure and an operational scanner
        // failure each project their own refusal reason — neither is ever
        // readable as a human rejection or a policy block.
        assert_eq!(
            projection_reason(Some(&SettlementDecision::Refusal(
                RefusalClass::ApprovalError
            ))),
            "refusal:approval_evaluation_error"
        );
        assert_eq!(
            projection_reason(Some(&SettlementDecision::Refusal(
                RefusalClass::ScannerError
            ))),
            "refusal:output_scan_error"
        );
    }
}
