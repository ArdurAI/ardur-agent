//! Shared, interior-mutable wrappers that let the `&self` [`submit`] path mutate
//! state two substrate crates expect to own by value.
//!
//! - [`SharedDenyList`] wraps a [`HashSetDenyList`] so the cap-token verifier and
//!   the runtime's [`revoke_cap_token`] entry point write the *same* revocation
//!   set — a token revoked mid-session is denied on the next turn.
//! - [`SharedBudget`] wraps an [`InMemoryBudgetStore`] so the cost gate and the
//!   runtime's [`remaining_budget`] query observe the *same* ledger.
//!
//! Both satisfy the orphan rule: the foreign traits ([`DenyList`],
//! [`BudgetStore`]) are implemented for these *local* newtypes, not for a bare
//! `Arc<…>`.
//!
//! [`submit`]: crate::FusedRuntime::submit
//! [`revoke_cap_token`]: crate::FusedRuntime::revoke_cap_token
//! [`remaining_budget`]: crate::FusedRuntime::remaining_budget

use std::io;
use std::path::Path;
use std::sync::Arc;

use ardur_cap_token::{DenyList, FileDenyList, HashSetDenyList};
use ardur_cost_gate::{
    BudgetStore, CostDelta, CostEnvelope, CostTuple, HolderId, ReservationHandle,
};
use async_trait::async_trait;
use parking_lot::Mutex;

/// The revocation backend behind a [`SharedDenyList`].
enum DenyBackend {
    /// Process-local: revocation lives only as long as the process.
    Memory(HashSetDenyList),
    /// Durable: every revocation is appended to (and synced into) a hex-line
    /// file; lookups reload the file, so a revocation written by one process
    /// is visible to any other process that opened the same path
    /// ([`FileDenyList`]'s contract).
    File(FileDenyList),
}

/// A revocation deny-list shared between the cap-token verifier and the
/// runtime. Cloning shares the underlying backend, so revoking through one
/// handle is visible to every other.
///
/// The default ([`new`](Self::new)) is in-memory: revocation lasts only for
/// the life of the process. [`open_file`](Self::open_file) makes revocation
/// durable across restarts and visible to other processes sharing the path
/// (Epic 3, gh#361).
#[derive(Clone)]
pub struct SharedDenyList(Arc<Mutex<DenyBackend>>);

impl Default for SharedDenyList {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedDenyList {
    /// An empty in-memory shared deny-list (process-local revocation).
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(DenyBackend::Memory(
            HashSetDenyList::new(),
        ))))
    }

    /// Open (or create) a durable, file-backed shared deny-list at `path`.
    ///
    /// Every revocation through this handle is appended to the file and
    /// synced; lookups reload the file, so revocations survive a restart and
    /// are visible to any other process that opened the same path.
    ///
    /// # Errors
    /// Returns an I/O error if the file cannot be created or read (see
    /// [`FileDenyList::open`]).
    pub fn open_file(path: impl AsRef<Path>) -> io::Result<Self> {
        let backend = FileDenyList::open(path.as_ref().to_path_buf())?;
        Ok(Self(Arc::new(Mutex::new(DenyBackend::File(backend)))))
    }

    /// Revoke every revocation id carried by `token`. Subsequent verifies that
    /// consult this list will reject the token with
    /// [`CapTokenError::Revoked`](ardur_cap_token::CapTokenError::Revoked).
    ///
    /// # Errors
    /// With the durable backend this persists to disk; an I/O failure means
    /// the revocation did NOT land and the caller must not report success.
    /// The in-memory backend is infallible.
    pub fn revoke_token(&self, token: &ardur_cap_token::CapToken) -> io::Result<()> {
        match &mut *self.0.lock() {
            DenyBackend::Memory(list) => {
                list.revoke_token(token);
                Ok(())
            }
            DenyBackend::File(list) => list.revoke_token(token),
        }
    }
}

impl DenyList for SharedDenyList {
    fn is_revoked(&self, revocation_ids: &[Vec<u8>]) -> bool {
        match &*self.0.lock() {
            DenyBackend::Memory(list) => list.is_revoked(revocation_ids),
            DenyBackend::File(list) => list.is_revoked(revocation_ids),
        }
    }
}

/// A budget ledger shared between the cost gate (which owns its [`BudgetStore`]
/// by value) and the runtime (which answers balance queries against the same
/// accounts). Cloning shares the underlying store.
#[derive(Clone, Default)]
pub struct SharedBudget(Arc<ardur_cost_gate::InMemoryBudgetStore>);

impl SharedBudget {
    /// An empty shared budget store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Provision (or overwrite) a holder's balance.
    pub fn set_balance(&self, holder: HolderId, balance: CostTuple) {
        self.0.set_balance(holder, balance);
    }

    /// Synchronous refund (delegates to
    /// [`InMemoryBudgetStore::refund_sync`]); see that method for why the cost
    /// gate needs a sync path (ARD-488).
    pub fn refund_sync(
        &self,
        handle: ReservationHandle,
        delta: CostDelta,
    ) -> Result<(), ardur_cost_gate::BudgetError> {
        self.0.refund_sync(handle, delta)
    }
}

#[async_trait]
impl BudgetStore for SharedBudget {
    async fn current_balance(
        &self,
        holder: &HolderId,
    ) -> Result<CostTuple, ardur_cost_gate::BudgetError> {
        self.0.current_balance(holder).await
    }

    async fn try_reserve(
        &self,
        holder: &HolderId,
        envelope: &CostEnvelope,
    ) -> Result<ReservationHandle, ardur_cost_gate::BudgetError> {
        self.0.try_reserve(holder, envelope).await
    }

    async fn refund(
        &self,
        handle: ReservationHandle,
        delta: CostDelta,
    ) -> Result<(CostTuple, CostTuple), ardur_cost_gate::BudgetError> {
        self.0.refund(handle, delta).await
    }

    async fn provision_merge(
        &self,
        holder: &HolderId,
        add: &CostTuple,
        cap: Option<&CostTuple>,
    ) -> Result<CostTuple, ardur_cost_gate::BudgetError> {
        self.0.provision_merge(holder, add, cap).await
    }
}
