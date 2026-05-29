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
use crate::error::{AdmissionError, BudgetError};
use crate::types::{
    AdmissionRequest, CostDelta, CostEnvelope, CostTuple, ProviderId, RefundReceipt, Reservation,
    ReservationHandle, ReservationStatus, UnixTsMillis,
};

/// The admission gate every metered call routes through. [`admit`](Self::admit)
/// runs stages 1–3 and hands back a [`Reservation`]; [`finalize`](Self::finalize)
/// runs stage 4 against that reservation.
#[async_trait]
pub trait CostAdmissionGate: Send + Sync {
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
}

/// Default reservation lifetime: how long a hold survives without a finalize.
const DEFAULT_TTL_MS: u64 = 30_000;

struct ReservationRecord {
    handle: ReservationHandle,
    expires_at: UnixTsMillis,
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
    reservations: RwLock<HashMap<Uuid, ReservationRecord>>,
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
            reservations: RwLock::new(HashMap::new()),
        }
    }

    /// Set how long a reservation survives without a finalize.
    pub fn with_ttl_ms(mut self, ttl_ms: u64) -> Self {
        self.ttl_ms = ttl_ms;
        self
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
            },
        );
        Ok(reservation)
    }

    async fn finalize(
        &self,
        reservation: Reservation,
        actual: CostTuple,
    ) -> Result<RefundReceipt, AdmissionError> {
        // Pull the live record (and its store handle) out under the lock, and
        // decide expiry against the same clock the reservation was stamped with.
        // Removing it makes a second finalize a no-such-reservation error.
        let (handle, expired) = {
            let mut reservations = self.reservations.write();
            let record = reservations
                .remove(&reservation.reservation_id)
                .ok_or_else(|| {
                    AdmissionError::Internal(anyhow::anyhow!(
                        "no active reservation for {}",
                        reservation.reservation_id
                    ))
                })?;
            let expired = self.clock.now_ms() > record.expires_at;
            (record.handle, expired)
        };

        let reserved = handle.reserved;
        if expired {
            // Release the entire hold so the budget is not stranded, then report
            // the expiry to the caller.
            self.budget
                .refund(handle, CostDelta::full_credit(&reserved))
                .await
                .map_err(internal)?;
            return Err(AdmissionError::ReservationExpired);
        }

        let refunded = CostDelta::between(&reserved, &actual);
        self.budget
            .refund(handle, refunded)
            .await
            .map_err(internal)?;
        Ok(RefundReceipt {
            reservation_id: reservation.reservation_id,
            actual,
            refunded,
            finalized_at: self.clock.now_ms(),
        })
    }
}
