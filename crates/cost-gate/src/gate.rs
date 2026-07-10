//! The cost-admission gate: [`CostAdmissionGate`] and its in-memory
//! implementation realizing ADR-Phase3-548's four stages.
//!
//! - **Stage 1 — project envelope.** Take the request's projected
//!   [`CostEnvelope`] and resolve its cap-token to a [`HolderId`].
//! - **Stage 2 — check ceilings.** Screen the provider against the allowlist and
//!   the envelope against the hard ceiling (Phase-1 stand-in for Cedar policy).
//! - **Stage 3 — reserve envelope.** Atomically decrement the holder's budget
//!   by the envelope via [`BudgetStore::try_reserve`], producing a [`Reservation`].
//! - **Stage 4 — finalize + refund.** [`CostAdmissionGate::finalize`] posts the
//!   actual cost and refunds the `reserved - actual` delta.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::HolderId;
use crate::budget::BudgetStore;
use crate::clock::{Clock, SystemClock};
use crate::error::{AdmissionError, BudgetError, ProvisionError};
use crate::types::{
    AdmissionRequest, CostDelta, CostEnvelope, CostTuple, ProviderId, RefundReceipt, Reservation,
    ReservationHandle, ReservationStatus, UnixTsMillis,
};

/// The admission gate every metered call routes through. [`admit`](Self::admit)
/// runs stages 1–3 and hands back a [`Reservation`]; [`finalize`](Self::finalize)
/// runs stage 4 against that reservation.
#[async_trait]
pub trait CostAdmissionGate: Send + Sync {
    /// Request-time provisioning: top `subject` up by `budget`, creating the
    /// account if it does not exist yet. The merge policy is **additive** — the
    /// new budget sums onto whatever balance remains — so a server that tops a
    /// holder up per turn never discards the holder's unspent budget the way a
    /// replace would. Idempotent in the sense that the operation is well-defined
    /// for an already-provisioned subject (it accumulates); it is *not*
    /// idempotent in the set-once sense. A merge that would breach the gate's
    /// configured per-subject cap fails with [`ProvisionError::OverCap`] and
    /// leaves the balance unchanged.
    async fn provision_for(
        &self,
        subject: &HolderId,
        budget: CostTuple,
    ) -> Result<(), ProvisionError>;

    /// Stages 1–3: project the envelope, check ceilings, and reserve the
    /// envelope against the holder's budget.
    async fn admit(&self, req: AdmissionRequest) -> Result<Reservation, AdmissionError>;

    /// Stage 4: post the `actual` cost and refund the unspent delta, returning
    /// a [`RefundReceipt`]. Fails with [`AdmissionError::ReservationExpired`] if
    /// the reservation lapsed first.
    async fn finalize(
        &self,
        reservation: Reservation,
        actual: CostTuple,
    ) -> Result<RefundReceipt, AdmissionError>;

    /// Roll back a successful finalization when no authoritative receipt was
    /// persisted. Exactly one rollback is accepted for a finalized reservation.
    async fn rollback_finalization(&self, receipt: RefundReceipt) -> Result<(), AdmissionError>;

    /// Forget rollback state after the corresponding receipt became durable.
    async fn commit_finalization(&self, reservation_id: Uuid);
}

/// Default reservation lifetime: how long a hold survives without a finalize.
const DEFAULT_TTL_MS: u64 = 30_000;

struct ReservationRecord {
    handle: ReservationHandle,
    expires_at: UnixTsMillis,
    /// `true` when a finalize call has claimed this reservation and is awaiting
    /// the budget refund. The cancel guard must not release a finalizing
    /// reservation — the finalize path owns the refund.
    finalizing: bool,
}

struct FinalizedRecord {
    handle: ReservationHandle,
    rollback_credit: CostTuple,
}

fn rollback_credit(
    reserved: CostTuple,
    balance_before: CostTuple,
    balance_after: CostTuple,
) -> CostTuple {
    fn dimension(reserved: u64, before: u64, after: u64) -> u64 {
        if after >= before {
            reserved.saturating_sub(after - before)
        } else {
            reserved.saturating_add(before - after)
        }
    }

    CostTuple {
        tokens_in: dimension(
            reserved.tokens_in,
            balance_before.tokens_in,
            balance_after.tokens_in,
        ),
        tokens_out: dimension(
            reserved.tokens_out,
            balance_before.tokens_out,
            balance_after.tokens_out,
        ),
        cents: dimension(reserved.cents, balance_before.cents, balance_after.cents),
        wall_ms: dimension(
            reserved.wall_ms,
            balance_before.wall_ms,
            balance_after.wall_ms,
        ),
        attention_score: dimension(
            reserved.attention_score,
            balance_before.attention_score,
            balance_after.attention_score,
        ),
    }
}

/// In-memory [`CostAdmissionGate`] over any [`BudgetStore`]. Holds an active-
/// reservation table (so [`finalize`](Self::finalize) can recover the store
/// handle) and a Phase-1 cap-token→holder directory.
pub struct InMemoryCostAdmissionGate<B: BudgetStore> {
    budget: B,
    clock: Arc<dyn Clock>,
    ttl_ms: u64,
    token_holders: RwLock<HashMap<crate::types::TokenId, HolderId>>,
    allowed_providers: Option<HashSet<ProviderId>>,
    ceiling: Option<CostEnvelope>,
    provision_cap: Option<CostTuple>,
    reservations: RwLock<HashMap<Uuid, ReservationRecord>>,
    finalized: RwLock<HashMap<Uuid, FinalizedRecord>>,
}

impl<B: BudgetStore> InMemoryCostAdmissionGate<B> {
    /// A gate over `budget` using the system clock and default TTL, with no
    /// provider allowlist and no hard ceiling.
    pub fn new(budget: B) -> Self {
        Self::with_clock(budget, Arc::new(SystemClock))
    }

    /// A gate with an explicit clock (for deterministic expiry in tests).
    pub fn with_clock(budget: B, clock: Arc<dyn Clock>) -> Self {
        Self {
            budget,
            clock,
            ttl_ms: DEFAULT_TTL_MS,
            token_holders: RwLock::new(HashMap::new()),
            allowed_providers: None,
            ceiling: None,
            provision_cap: None,
            reservations: RwLock::new(HashMap::new()),
            finalized: RwLock::new(HashMap::new()),
        }
    }

    /// Set how long a reservation survives without a finalize.
    pub fn with_ttl_ms(mut self, ttl_ms: u64) -> Self {
        self.ttl_ms = ttl_ms;
        self
    }

    /// Remove and return the reservation handle for `reservation_id`, if it is
    /// still active (idempotent: `None` if already finalized/expired/cancelled).
    /// Lets a release-on-drop guard (ARD-488) claim a cancelled turn's
    /// reservation synchronously and refund it through whatever budget store the
    /// gate shares — without an await point, so it can run from `Drop`.
    pub fn take_reservation(&self, reservation_id: Uuid) -> Option<ReservationHandle> {
        let mut reservations = self.reservations.write();
        let record = reservations.get(&reservation_id)?;
        if record.finalizing {
            return None;
        }
        reservations
            .remove(&reservation_id)
            .map(|record| record.handle)
    }

    /// Restrict admission to these providers (an empty set denies all). Without
    /// this, every provider is allowed.
    pub fn with_allowed_providers(mut self, providers: HashSet<ProviderId>) -> Self {
        self.allowed_providers = Some(providers);
        self
    }

    /// Impose a hard per-call ceiling: any envelope exceeding it on any
    /// dimension is [`AdmissionError::PolicyDenied`].
    pub fn with_ceiling(mut self, ceiling: CostEnvelope) -> Self {
        self.ceiling = Some(ceiling);
        self
    }

    /// Cap the *accumulated* balance any single subject may be provisioned to.
    /// A [`provision_for`](CostAdmissionGate::provision_for) whose additive merge
    /// would push a subject's balance past this on any dimension is refused with
    /// [`ProvisionError::OverCap`]. Without this, top-ups are unbounded.
    pub fn with_provision_cap(mut self, cap: CostTuple) -> Self {
        self.provision_cap = Some(cap);
        self
    }

    /// Phase-1 stand-in for cap-token holder resolution: bind a token id to the
    /// holder whose budget it spends.
    // TODO §11.14 Phase 2: resolve the holder from the verified cap-token
    // (Biscuit) claims instead of this explicit directory.
    pub fn bind_token(&self, token_id: crate::types::TokenId, holder: HolderId) {
        self.token_holders.write().insert(token_id, holder);
    }
}

/// True if `env` exceeds `ceiling` on any dimension.
fn exceeds(env: &CostEnvelope, ceiling: &CostEnvelope) -> bool {
    env.tokens_in_max > ceiling.tokens_in_max
        || env.tokens_out_max > ceiling.tokens_out_max
        || env.cents_max > ceiling.cents_max
        || env.wall_ms_max > ceiling.wall_ms_max
        || env.attention_score_max > ceiling.attention_score_max
}

fn clamp_u32(v: u64) -> u32 {
    v.min(u64::from(u32::MAX)) as u32
}

/// Demote a store error to an admission-level internal error, preserving an
/// already-wrapped inner cause.
fn internal(e: BudgetError) -> AdmissionError {
    match e {
        BudgetError::Internal(inner) => AdmissionError::Internal(inner),
        other => AdmissionError::Internal(anyhow::Error::new(other)),
    }
}

#[async_trait]
impl<B: BudgetStore> CostAdmissionGate for InMemoryCostAdmissionGate<B> {
    async fn provision_for(
        &self,
        subject: &HolderId,
        budget: CostTuple,
    ) -> Result<(), ProvisionError> {
        match self
            .budget
            .provision_merge(subject, &budget, self.provision_cap.as_ref())
            .await
        {
            Ok(_) => Ok(()),
            Err(BudgetError::OverProvisionCap(dimension)) => Err(ProvisionError::OverCap {
                subject: subject.clone(),
                dimension,
            }),
            Err(other) => Err(ProvisionError::Internal(anyhow::Error::new(other))),
        }
    }

    async fn admit(&self, req: AdmissionRequest) -> Result<Reservation, AdmissionError> {
        // Stage 1 — project envelope + resolve the holder from the cap-token.
        let envelope = req.projected_envelope;
        let holder = self
            .token_holders
            .read()
            .get(&req.cap_token_id)
            .cloned()
            .ok_or(AdmissionError::CapTokenInvalid)?;

        // Stage 2 — check ceilings (Phase-1 stand-in for the Cedar policy).
        if self
            .allowed_providers
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(&req.provider_id))
        {
            return Err(AdmissionError::ProviderNotAllowed(req.provider_id));
        }
        if self
            .ceiling
            .as_ref()
            .is_some_and(|ceiling| exceeds(&envelope, ceiling))
        {
            return Err(AdmissionError::PolicyDenied(
                "projected envelope exceeds the per-call ceiling".to_string(),
            ));
        }
        // TODO §11.14 Phase 2: replace stage 2 with Cedar policy evaluation over
        // per-org ceilings (the provider allowlist + hard ceiling become facts).

        // Stage 3 — reserve the envelope against the holder's budget.
        let need = CostTuple::from_envelope(&envelope);
        let balance = match self.budget.current_balance(&holder).await {
            Ok(b) => b,
            Err(BudgetError::HolderNotFound) => {
                return Err(AdmissionError::BudgetExhausted {
                    required: envelope.cents_max,
                    available: 0,
                });
            }
            Err(other) => return Err(internal(other)),
        };
        if !balance.covers(&need) {
            return Err(AdmissionError::BudgetExhausted {
                required: envelope.cents_max,
                available: clamp_u32(balance.cents),
            });
        }
        let handle = match self.budget.try_reserve(&holder, &envelope).await {
            Ok(h) => h,
            // A concurrent reservation claimed the budget between the check and
            // the decrement — exhausted from this caller's view.
            Err(BudgetError::RaceLost) => {
                let available = clamp_u32(
                    self.budget
                        .current_balance(&holder)
                        .await
                        .map(|b| b.cents)
                        .unwrap_or(0),
                );
                return Err(AdmissionError::BudgetExhausted {
                    required: envelope.cents_max,
                    available,
                });
            }
            Err(other) => return Err(internal(other)),
        };

        let now = self.clock.now_ms();
        let reservation = Reservation {
            reservation_id: Uuid::new_v4(),
            cap_token_id: req.cap_token_id,
            envelope,
            reserved_at: now,
            expires_at: now.saturating_add(self.ttl_ms),
            status: ReservationStatus::Active,
        };
        self.reservations.write().insert(
            reservation.reservation_id,
            ReservationRecord {
                handle,
                expires_at: reservation.expires_at,
                finalizing: false,
            },
        );
        Ok(reservation)
    }

    async fn finalize(
        &self,
        reservation: Reservation,
        actual: CostTuple,
    ) -> Result<RefundReceipt, AdmissionError> {
        // ARD-448: Finalization is an atomic claim of the reservation.
        //
        // The previous implementation peeked the reservation under a read lock,
        // awaited the budget refund, and removed the reservation afterward. Two
        // concurrent finalize calls could both observe the same active reservation
        // before either removed it, causing the same refund delta to be credited
        // twice. Claim by removing under the write lock first; exactly one caller
        // receives the handle and every later caller sees "no active reservation".
        let now = self.clock.now_ms();
        let (handle, expired) = {
            let mut reservations = self.reservations.write();
            let record = reservations
                .get_mut(&reservation.reservation_id)
                .ok_or_else(|| {
                    AdmissionError::Internal(anyhow::anyhow!(
                        "no active reservation for {}",
                        reservation.reservation_id
                    ))
                })?;
            if record.finalizing {
                return Err(AdmissionError::Internal(anyhow::anyhow!(
                    "reservation {} is already being finalized",
                    reservation.reservation_id
                )));
            }
            record.finalizing = true;
            (record.handle.clone(), now > record.expires_at)
        };

        let reserved = handle.reserved;

        if expired {
            self.reservations
                .write()
                .remove(&reservation.reservation_id);
            self.budget
                .refund(handle, CostDelta::full_credit(&reserved))
                .await
                .map_err(internal)?;
            return Err(AdmissionError::ReservationExpired);
        }

        // ARD-296: Combine base cost + tool execution cost for post-receipt hooks.
        // The `actual` CostTuple already includes all costs (LLM + tools) if the
        // caller aggregated them before calling finalize. Here we ensure the
        // receipt reflects the combined cost, not just the provider-reported cost.
        //
        // The refund delta is `reserved - actual_combined`. If the combined cost
        // exceeds the reserved envelope, the delta will be negative (debiting the
        // overage from the holder's budget), which is the correct fail-closed behavior.
        let refunded = CostDelta::between(&reserved, &actual);
        let rollback_handle = handle.clone();
        let (balance_before, balance_after) = self
            .budget
            .refund(handle, refunded)
            .await
            .map_err(internal)?;
        let rollback_credit = rollback_credit(reserved, balance_before, balance_after);
        self.reservations
            .write()
            .remove(&reservation.reservation_id);
        self.finalized.write().insert(
            reservation.reservation_id,
            FinalizedRecord {
                handle: rollback_handle,
                rollback_credit,
            },
        );

        Ok(RefundReceipt {
            reservation_id: reservation.reservation_id,
            actual,
            refunded,
            finalized_at: self.clock.now_ms(),
        })
    }

    async fn rollback_finalization(&self, receipt: RefundReceipt) -> Result<(), AdmissionError> {
        let record = self
            .finalized
            .write()
            .remove(&receipt.reservation_id)
            .ok_or_else(|| {
                AdmissionError::Internal(anyhow::anyhow!(
                    "no rollback state for finalized reservation {}",
                    receipt.reservation_id
                ))
            })?;
        self.budget
            .refund(
                record.handle,
                CostDelta::full_credit(&record.rollback_credit),
            )
            .await
            .map(|_| ())
            .map_err(internal)
    }

    async fn commit_finalization(&self, reservation_id: Uuid) {
        self.finalized.write().remove(&reservation_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::InMemoryBudgetStore;
    use crate::clock::ManualClock;
    use crate::types::{ModelId, Sha256Digest, TokenId};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Notify;

    fn test_gate() -> (
        InMemoryCostAdmissionGate<InMemoryBudgetStore>,
        Arc<ManualClock>,
        HolderId,
        TokenId,
    ) {
        let clock = Arc::new(ManualClock::new(0));
        let budget = InMemoryBudgetStore::new();
        let gate = InMemoryCostAdmissionGate::with_clock(budget, clock.clone());
        let holder = HolderId("test".to_string());
        let token_id = TokenId(Uuid::new_v4());
        gate.bind_token(token_id, holder.clone());
        (gate, clock, holder, token_id)
    }

    fn req(envelope: CostEnvelope, token_id: TokenId) -> AdmissionRequest {
        AdmissionRequest {
            cap_token_id: token_id,
            projected_envelope: envelope,
            provider_id: ProviderId("openrouter".to_string()),
            model_id: ModelId("gpt-4".to_string()),
            request_digest: Sha256Digest::of(b"test"),
        }
    }

    struct DelayedRefundBudgetStore {
        inner: InMemoryBudgetStore,
        first_refund_started: Arc<Notify>,
        refund_calls: Arc<AtomicUsize>,
    }

    impl DelayedRefundBudgetStore {
        fn new() -> Self {
            Self {
                inner: InMemoryBudgetStore::new(),
                first_refund_started: Arc::new(Notify::new()),
                refund_calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait::async_trait]
    impl BudgetStore for DelayedRefundBudgetStore {
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
            let previous = self.refund_calls.fetch_add(1, Ordering::SeqCst);
            if previous == 0 {
                self.first_refund_started.notify_waiters();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
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

    #[tokio::test]
    async fn finalize_fail_closed_budget_unchanged() {
        let (gate, _clock, holder, token_id) = test_gate();
        gate.provision_for(
            &holder,
            CostTuple {
                tokens_in: 100,
                tokens_out: 100,
                cents: 100,
                wall_ms: 1000,
                attention_score: 10,
            },
        )
        .await
        .unwrap();

        let envelope = CostEnvelope {
            tokens_in_max: 10,
            tokens_out_max: 10,
            cents_max: 50,
            wall_ms_max: 1000,
            attention_score_max: 1,
        };
        let r = req(envelope, token_id);

        let reservation = gate.admit(r).await.unwrap();

        let actual = CostTuple {
            tokens_in: 5,
            tokens_out: 5,
            cents: 20,
            wall_ms: 500,
            attention_score: 0,
        };
        let receipt = gate.finalize(reservation, actual).await.unwrap();

        assert_eq!(receipt.actual.cents, 20);
        assert_eq!(receipt.refunded.cents, 30);

        let balance_after = gate.budget.current_balance(&holder).await.unwrap();
        assert_eq!(balance_after.cents, 80);
    }

    #[tokio::test]
    async fn rollback_of_clamped_overrun_restores_without_inflation() {
        let (gate, _clock, holder, token_id) = test_gate();
        let initial = CostTuple {
            tokens_in: 15,
            tokens_out: 15,
            cents: 15,
            wall_ms: 15,
            attention_score: 15,
        };
        gate.provision_for(&holder, initial).await.unwrap();
        let reservation = gate
            .admit(req(
                CostEnvelope {
                    tokens_in_max: 10,
                    tokens_out_max: 10,
                    cents_max: 10,
                    wall_ms_max: 10,
                    attention_score_max: 10,
                },
                token_id,
            ))
            .await
            .unwrap();
        let actual = CostTuple {
            tokens_in: 20,
            tokens_out: 20,
            cents: 20,
            wall_ms: 20,
            attention_score: 20,
        };
        let receipt = gate.finalize(reservation, actual).await.unwrap();
        assert_eq!(
            gate.budget.current_balance(&holder).await.unwrap(),
            CostTuple::ZERO,
            "the overrun debit clamps each dimension at zero"
        );

        gate.rollback_finalization(receipt).await.unwrap();
        assert_eq!(
            gate.budget.current_balance(&holder).await.unwrap(),
            initial,
            "rollback restores exactly the applied charge, not the larger requested actual"
        );
    }

    #[tokio::test]
    async fn finalize_expired_releases_full_hold() {
        let (gate, clock, holder, token_id) = test_gate();
        gate.provision_for(
            &holder,
            CostTuple {
                tokens_in: 100,
                tokens_out: 100,
                cents: 100,
                wall_ms: 1000,
                attention_score: 10,
            },
        )
        .await
        .unwrap();

        let envelope = CostEnvelope {
            tokens_in_max: 10,
            tokens_out_max: 10,
            cents_max: 50,
            wall_ms_max: 1000,
            attention_score_max: 1,
        };
        let r = req(envelope, token_id);

        let reservation = gate.admit(r).await.unwrap();
        let balance_after_reserve = gate.budget.current_balance(&holder).await.unwrap();
        assert_eq!(balance_after_reserve.cents, 50);

        clock.advance(31_000);

        let actual = CostTuple {
            tokens_in: 5,
            tokens_out: 5,
            cents: 20,
            wall_ms: 500,
            attention_score: 0,
        };
        let result = gate.finalize(reservation, actual).await;

        assert!(matches!(result, Err(AdmissionError::ReservationExpired)));

        let balance_after_expiry = gate.budget.current_balance(&holder).await.unwrap();
        assert_eq!(balance_after_expiry.cents, 100);
    }

    #[tokio::test]
    async fn finalize_combined_cost_in_receipt() {
        let (gate, _clock, holder, token_id) = test_gate();
        gate.provision_for(
            &holder,
            CostTuple {
                tokens_in: 100,
                tokens_out: 100,
                cents: 100,
                wall_ms: 1000,
                attention_score: 10,
            },
        )
        .await
        .unwrap();

        let envelope = CostEnvelope {
            tokens_in_max: 10,
            tokens_out_max: 10,
            cents_max: 50,
            wall_ms_max: 1000,
            attention_score_max: 1,
        };
        let r = req(envelope, token_id);

        let reservation = gate.admit(r).await.unwrap();

        let actual = CostTuple {
            tokens_in: 5,
            tokens_out: 5,
            cents: 30,
            wall_ms: 500,
            attention_score: 0,
        };
        let receipt = gate.finalize(reservation, actual).await.unwrap();

        assert_eq!(receipt.actual.cents, 30);
        assert_eq!(receipt.refunded.cents, 20);
    }

    #[tokio::test]
    async fn concurrent_double_finalize_credits_refund_once() {
        let clock = Arc::new(ManualClock::new(0));
        let budget = DelayedRefundBudgetStore::new();
        let first_refund_started = budget.first_refund_started.clone();
        let refund_calls = budget.refund_calls.clone();
        let gate = Arc::new(InMemoryCostAdmissionGate::with_clock(budget, clock));
        let holder = HolderId("test".to_string());
        let token_id = TokenId(Uuid::new_v4());
        gate.bind_token(token_id, holder.clone());
        gate.provision_for(
            &holder,
            CostTuple {
                tokens_in: 100,
                tokens_out: 100,
                cents: 100,
                wall_ms: 1000,
                attention_score: 10,
            },
        )
        .await
        .unwrap();

        let envelope = CostEnvelope {
            tokens_in_max: 10,
            tokens_out_max: 10,
            cents_max: 50,
            wall_ms_max: 1000,
            attention_score_max: 1,
        };
        let reservation = gate.admit(req(envelope, token_id)).await.unwrap();
        let actual = CostTuple {
            tokens_in: 5,
            tokens_out: 5,
            cents: 20,
            wall_ms: 500,
            attention_score: 0,
        };

        let first_refund_observed = first_refund_started.notified();
        let first_gate = gate.clone();
        let first_reservation = reservation.clone();
        let first_finalize =
            tokio::spawn(async move { first_gate.finalize(first_reservation, actual).await });
        first_refund_observed.await;

        let second_result = gate.finalize(reservation, actual).await;
        let first_result = first_finalize.await.unwrap();

        let success_count = [first_result.is_ok(), second_result.is_ok()]
            .into_iter()
            .filter(|success| *success)
            .count();
        assert_eq!(
            success_count, 1,
            "exactly one finalizer should claim the reservation"
        );
        assert_eq!(
            refund_calls.load(Ordering::SeqCst),
            1,
            "refund must be called once"
        );

        let balance_after = gate.budget.current_balance(&holder).await.unwrap();
        assert_eq!(
            balance_after.cents, 80,
            "budget should receive one 30c refund"
        );
    }

    #[tokio::test]
    async fn concurrent_reservations_race_safe() {
        let (gate, _clock, holder, token_id) = test_gate();
        gate.provision_for(
            &holder,
            CostTuple {
                tokens_in: 100,
                tokens_out: 100,
                cents: 100,
                wall_ms: 1000,
                attention_score: 10,
            },
        )
        .await
        .unwrap();

        let envelope = CostEnvelope {
            tokens_in_max: 10,
            tokens_out_max: 10,
            cents_max: 60,
            wall_ms_max: 1000,
            attention_score_max: 1,
        };

        let r1 = req(envelope, token_id);
        let r2 = req(envelope, token_id);
        // Note: both requests use the same token, so they compete for the same budget.
        // This is intentional for the race test.

        let (res1, res2) = tokio::join!(gate.admit(r1), gate.admit(r2));

        let success_count = [res1.is_ok(), res2.is_ok()].iter().filter(|&&x| x).count();

        assert!(
            success_count <= 1,
            "At most one reservation should succeed with 100c budget and 60c requests"
        );
    }
}
