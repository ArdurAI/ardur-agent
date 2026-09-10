//! [`JournalRegistry`] — [`SessionId`]→journal resolution.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::error::RegistryError;
use crate::journal::SessionJournal;
use ardur_runtime::SessionId;

/// A process-wide registry resolving a [`SessionJournal`] by [`SessionId`].
///
/// [`get_or_create`](Self::get_or_create) is the primary entry point: it returns
/// the journal already serving a session, or mints one with the caller's factory
/// — so two callers racing on the same in-flight session share one journal
/// rather than splitting its log across two.
///
/// # Lifetime model
///
/// The registry stores each journal as an [`Arc<dyn SessionJournal>`] and hands
/// out clones of that handle, so a journal lives exactly as long as the registry
/// entry plus any handles still in flight — not for the life of the process.
/// When a session ends, [`remove`](Self::remove) drops the registry's handle;
/// once the last outstanding clone is dropped the journal is destructed and its
/// backing resources (for [`FileSessionJournal`](crate::FileSessionJournal), the
/// open file descriptor) are reclaimed. A long-running server therefore holds
/// journals only for live sessions rather than accumulating one — and one FD —
/// per session ever seen.
#[derive(Default)]
pub struct JournalRegistry {
    journals: Mutex<HashMap<SessionId, Arc<dyn SessionJournal>>>,
}

impl JournalRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `journal` under the [`SessionId`] it owns.
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::AlreadyRegistered`] if a journal is already
    /// registered for that session; the registry never silently replaces one.
    pub fn register(&self, journal: Arc<dyn SessionJournal>) -> Result<(), RegistryError> {
        let session_id = *journal.session_id();
        let mut journals = self.journals.lock();
        if journals.contains_key(&session_id) {
            return Err(RegistryError::AlreadyRegistered(session_id));
        }
        journals.insert(session_id, journal);
        Ok(())
    }

    /// Resolve the journal for `session_id`, or mint one with `factory` and
    /// register it. Calling this twice for the same session returns a clone of
    /// the same handle — the factory runs only on the first call.
    pub fn get_or_create<F>(
        &self,
        session_id: &SessionId,
        factory: F,
    ) -> Result<Arc<dyn SessionJournal>, RegistryError>
    where
        F: Fn() -> Arc<dyn SessionJournal>,
    {
        // Hold the lock across the check and the insert so two racing callers
        // resolve to the same journal rather than each minting one.
        let mut journals = self.journals.lock();
        if let Some(existing) = journals.get(session_id) {
            return Ok(Arc::clone(existing));
        }
        let journal = factory();
        journals.insert(*session_id, Arc::clone(&journal));
        Ok(journal)
    }

    /// Resolve the journal for `session_id` without minting one.
    ///
    /// Returns a clone of the stored handle, or `None` if no journal is
    /// registered for that session.
    #[must_use]
    pub fn get(&self, session_id: &SessionId) -> Option<Arc<dyn SessionJournal>> {
        self.journals.lock().get(session_id).map(Arc::clone)
    }

    /// Evict the journal for `session_id`, returning the removed handle.
    ///
    /// Call this when a session ends. The registry's handle is dropped
    /// immediately; the returned handle lets the caller flush and
    /// [`close`](crate::SessionJournal::close) the journal before releasing it.
    /// Once every outstanding clone is dropped the journal is destructed and its
    /// backing file descriptor (for
    /// [`FileSessionJournal`](crate::FileSessionJournal)) is reclaimed — so no
    /// journal or FD outlives its session.
    ///
    /// Returns `None` if no journal was registered for that session.
    pub fn remove(&self, session_id: &SessionId) -> Option<Arc<dyn SessionJournal>> {
        self.journals.lock().remove(session_id)
    }

    /// Whether a journal is registered for `session_id`.
    #[must_use]
    pub fn contains(&self, session_id: &SessionId) -> bool {
        self.journals.lock().contains_key(session_id)
    }

    /// The number of registered journals.
    #[must_use]
    pub fn len(&self) -> usize {
        self.journals.lock().len()
    }

    /// Whether the registry has no journals.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.journals.lock().is_empty()
    }
}
