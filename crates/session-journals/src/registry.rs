//! [`JournalRegistry`] — [`SessionId`]→journal resolution.

use std::collections::HashMap;

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
/// # Lifetime model (Phase 1)
///
/// The registry hands out `&dyn SessionJournal` borrows, so a registered
/// journal must outlive every borrow. Phase 1 keeps this simple by leaking each
/// journal into a `'static` reference: a session journal lives for the life of
/// the process, and the registry never evicts. Phase 2 replaces this with an
/// arena (or `Arc`-based handles) once journals acquire a lifecycle — close,
/// retention, and eviction (see the `// TODO §7.10 Phase 2:` markers in `lib.rs`).
#[derive(Default)]
pub struct JournalRegistry {
    journals: Mutex<HashMap<SessionId, &'static dyn SessionJournal>>,
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
    pub fn register(&self, journal: Box<dyn SessionJournal>) -> Result<(), RegistryError> {
        let session_id = *journal.session_id();
        let mut journals = self.journals.lock();
        if journals.contains_key(&session_id) {
            return Err(RegistryError::AlreadyRegistered(session_id));
        }
        // Leak to a 'static reference: a registered journal lives for the
        // process (Phase 1 — see the type-level lifetime note).
        journals.insert(session_id, Box::leak(journal));
        Ok(())
    }

    /// Resolve the journal for `session_id`, or mint one with `factory` and
    /// register it. Calling this twice for the same session returns the same
    /// journal — the factory runs only on the first call.
    pub fn get_or_create<F>(
        &self,
        session_id: &SessionId,
        factory: F,
    ) -> Result<&dyn SessionJournal, RegistryError>
    where
        F: Fn() -> Box<dyn SessionJournal>,
    {
        // Hold the lock across the check and the insert so two racing callers
        // resolve to the same journal rather than each minting one.
        let mut journals = self.journals.lock();
        if let Some(existing) = journals.get(session_id) {
            return Ok(*existing);
        }
        let leaked: &'static dyn SessionJournal = Box::leak(factory());
        journals.insert(*session_id, leaked);
        Ok(leaked)
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
