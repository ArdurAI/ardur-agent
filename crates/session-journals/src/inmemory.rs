//! [`InMemorySessionJournal`] — the Phase 1 default backend.

use async_trait::async_trait;
use parking_lot::RwLock;

use crate::error::JournalError;
use crate::journal::SessionJournal;
use crate::types::{EntryId, JournalEntry};
use ardur_runtime::SessionId;

/// An in-memory [`SessionJournal`]: the session's entries in a `Vec` behind a
/// non-poisoning [`RwLock`]. The Phase 1 default — durable only for the life of
/// the process, so it backs tests and ephemeral sessions; a session that must
/// survive a restart uses [`FileSessionJournal`](crate::FileSessionJournal).
///
/// An entry's [`EntryId`] is its index in the `Vec`, so ids are monotonic and
/// dense, and [`replay_from`](SessionJournal::replay_from) is a slice.
#[derive(Debug)]
pub struct InMemorySessionJournal {
    session_id: SessionId,
    entries: RwLock<Vec<JournalEntry>>,
}

impl InMemorySessionJournal {
    /// Create an empty in-memory journal for `session_id`.
    #[must_use]
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            entries: RwLock::new(Vec::new()),
        }
    }

    /// The number of entries currently recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.read().len()
    }

    /// Whether the journal has no entries yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.read().is_empty()
    }
}

#[async_trait]
impl SessionJournal for InMemorySessionJournal {
    async fn append(&self, entry: JournalEntry) -> Result<EntryId, JournalError> {
        let mut entries = self.entries.write();
        let id = EntryId::new(entries.len() as u64);
        entries.push(entry);
        Ok(id)
    }

    async fn replay(&self, session_id: SessionId) -> Result<Vec<JournalEntry>, JournalError> {
        if session_id != self.session_id {
            return Err(JournalError::SessionNotFound(session_id));
        }
        Ok(self.entries.read().clone())
    }

    async fn replay_from(
        &self,
        session_id: SessionId,
        from: EntryId,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        if session_id != self.session_id {
            return Err(JournalError::SessionNotFound(session_id));
        }
        let entries = self.entries.read();
        // `from` is exclusive, so the first returned entry is at `from + 1`.
        let start = from.value().saturating_add(1) as usize;
        if start > entries.len() {
            return Err(JournalError::EntryNotFound(from));
        }
        Ok(entries[start..].to_vec())
    }

    async fn close(&self) -> Result<(), JournalError> {
        // Nothing is buffered — an in-memory journal is durable only in-process.
        Ok(())
    }

    fn session_id(&self) -> &SessionId {
        &self.session_id
    }
}
