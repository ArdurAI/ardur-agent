//! The holder-keyed budget ledger and its in-memory implementation.
//!
//! [`InMemoryBudgetStore`] is optimistic: each account carries a `u64` version
//! bumped on every mutation. [`BudgetStore::try_reserve`] snapshots
//! `(balance, version)`, computes the post-decrement balance, then commits only
//! if the version is still current — retrying on conflict and surfacing
//! [`BudgetError::RaceLost`] only when the balance can no longer cover the
//! request. Because the balance falls monotonically under contention (refunds
//! aside), the retry loop terminates and the set of winners is determined by
//! capacity, not scheduling.

use std::collections::HashMap;

use async_trait::async_trait;
use parking_lot::RwLock;

use crate::HolderId;
use crate::error::BudgetError;
use crate::types::{CostDelta, CostEnvelope, CostTuple, ReservationHandle};

/// A budget ledger keyed by [`HolderId`]. The reserve/refund pair is the only
/// way a balance moves: a reservation atomically decrements, a finalize (or
/// expiry/cancellation) refunds the signed delta.
#[async_trait]
pub trait BudgetStore: Send + Sync {
    /// The holder's current remaining budget.
    async fn current_balance(&self, holder: &HolderId) -> Result<CostTuple, BudgetError>;

    /// Atomically check that the holder can cover `envelope` and decrement it,
    /// returning a [`ReservationHandle`] for the later refund. Fails with
    /// [`BudgetError::RaceLost`] if the balance cannot (any longer) cover it.
    async fn try_reserve(
        &self,
        holder: &HolderId,
        envelope: &CostEnvelope,
    ) -> Result<ReservationHandle, BudgetError>;

    /// Credit a signed `delta` back to the holder named by `handle` (the
    /// `reserved - actual` refund, or a full credit on release).
    async fn refund(&self, handle: ReservationHandle, delta: CostDelta) -> Result<(), BudgetError>;
}

struct Account {
    balance: CostTuple,
    version: u64,
}

/// An in-memory [`BudgetStore`] (a `HashMap` behind an `RwLock`, with a per-
/// account version for optimistic concurrency). The Phase-1 backend; Phase 2
/// swaps in a persistent store behind the same trait.
///
// TODO §11.14 Phase 2: replace with a persistent, per-org budget backend
// (the trait surface is the seam — callers depend only on `BudgetStore`).
#[derive(Default)]
pub struct InMemoryBudgetStore {
    accounts: RwLock<HashMap<HolderId, Account>>,
}

impl InMemoryBudgetStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Provision (or overwrite) a holder's balance. Bumps the account version.
    pub fn set_balance(&self, holder: HolderId, balance: CostTuple) {
        let mut accounts = self.accounts.write();
        let entry = accounts.entry(holder).or_insert(Account {
            balance: CostTuple::ZERO,
            version: 0,
        });
        entry.balance = balance;
        entry.version = entry.version.wrapping_add(1);
    }
}

#[async_trait]
impl BudgetStore for InMemoryBudgetStore {
    async fn current_balance(&self, holder: &HolderId) -> Result<CostTuple, BudgetError> {
        self.accounts
            .read()
            .get(holder)
            .map(|a| a.balance)
            .ok_or(BudgetError::HolderNotFound)
    }

    async fn try_reserve(
        &self,
        holder: &HolderId,
        envelope: &CostEnvelope,
    ) -> Result<ReservationHandle, BudgetError> {
        let need = CostTuple::from_envelope(envelope);
        loop {
            // Snapshot under a read lock.
            let (balance, version) = {
                let accounts = self.accounts.read();
                let acct = accounts.get(holder).ok_or(BudgetError::HolderNotFound)?;
                (acct.balance, acct.version)
            };

            // If the snapshot can't cover the request, a concurrent reservation
            // claimed the budget first — the race is lost (the balance only
            // falls, so re-reading would not help).
            let Some(post) = balance.checked_sub(&need) else {
                return Err(BudgetError::RaceLost);
            };

            // Commit iff the version is still the one we read.
            let mut accounts = self.accounts.write();
            let acct = accounts
                .get_mut(holder)
                .ok_or(BudgetError::HolderNotFound)?;
            if acct.version != version {
                continue; // someone mutated between snapshot and commit; retry
            }
            acct.balance = post;
            acct.version = acct.version.wrapping_add(1);
            return Ok(ReservationHandle {
                holder: holder.clone(),
                reserved: need,
                committed_version: acct.version,
            });
        }
    }

    async fn refund(&self, handle: ReservationHandle, delta: CostDelta) -> Result<(), BudgetError> {
        let mut accounts = self.accounts.write();
        let acct = accounts
            .get_mut(&handle.holder)
            .ok_or(BudgetError::HolderNotFound)?;
        acct.balance = acct.balance.apply_delta(&delta);
        acct.version = acct.version.wrapping_add(1);
        Ok(())
    }
}
