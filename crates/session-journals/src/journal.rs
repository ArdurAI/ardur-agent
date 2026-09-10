//! The [`SessionJournal`] contract — the object-safe async trait every backend
//! implements.

use async_trait::async_trait;

use crate::error::JournalError;
use crate::types::{EntryId, JournalEntry};
use ardur_runtime::SessionId;

/// The durable, replayable record of one agent session.
///
/// A journal is append-only: entries are written in order and never mutated in
/// place (a superseded entry is retracted with a
/// [`JournalEntry::Invalidation`](crate::JournalEntry::Invalidation), not
/// edited). [`append`](Self::append) returns the [`EntryId`] the entry landed
/// at; [`replay`](Self::replay) reads the whole log back; and
/// [`replay_from`](Self::replay_from) resumes after a checkpoint without
/// re-reading the history before it.
///
/// The trait is object-safe (`async-trait` boxes the returned futures) so an
/// `Arc<dyn SessionJournal>` can be stored in a [`JournalRegistry`](crate::JournalRegistry),
/// and `Send + Sync` so one journal can be shared across the tasks of a session.
#[async_trait]
pub trait SessionJournal: Send + Sync {
    /// Append `entry` to the end of the log, durably, and return the
    /// [`EntryId`] it landed at.
    ///
    /// A file-backed journal does a write-then-fsync so a returned EntryId names
    /// a durably persisted entry.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::Io`]/[`JournalError::Serde`] if the entry could
    /// not be persisted.
    async fn append(&self, entry: JournalEntry) -> Result<EntryId, JournalError>;

    /// Return the number of entries currently in this journal.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`replay`](Self::replay) for backends that
    /// compute length by reading the durable log.
    async fn len(&self) -> Result<u64, JournalError> {
        Ok(self.replay(*self.session_id()).await?.len() as u64)
    }

    /// Whether the journal currently has no entries.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`len`](Self::len).
    async fn is_empty(&self) -> Result<bool, JournalError> {
        Ok(self.len().await? == 0)
    }

    /// Truncate the journal to the first `len` entries, durably.
    ///
    /// This is used only by the two-phase receipt+journal commit path to roll
    /// back entries appended in a failed transaction before success is returned.
    /// Implementations must reject `len` values beyond the current tail.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::EntryNotFound`] if `len` is beyond the current
    /// tail, or backend-specific I/O errors if the truncation cannot be made
    /// durable.
    async fn truncate(&self, len: u64) -> Result<(), JournalError> {
        let _ = len;
        Err(JournalError::Malformed(
            "journal backend does not support transactional truncate".to_string(),
        ))
    }

    /// Replay the session's full log, in append order.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::SessionNotFound`] if `session_id` is not the
    /// session this journal owns, or an I/O/decode error if the log could not
    /// be read.
    async fn replay(&self, session_id: SessionId) -> Result<Vec<JournalEntry>, JournalError>;

    /// Replay the entries recorded *after* the `from` cursor — the resume path,
    /// where `from` is typically the [`EntryId`] of a
    /// [`JournalEntry::Checkpoint`](crate::JournalEntry::Checkpoint). `from` is
    /// exclusive: the entry at `from` is not returned.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::SessionNotFound`] if `session_id` is not the
    /// session this journal owns, [`JournalError::EntryNotFound`] if `from` is
    /// past the end of the log, or an I/O/decode error if the log could not be
    /// read.
    async fn replay_from(
        &self,
        session_id: SessionId,
        from: EntryId,
    ) -> Result<Vec<JournalEntry>, JournalError>;

    /// Flush any buffered state and close the journal.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::Io`] if the final flush failed.
    async fn close(&self) -> Result<(), JournalError>;

    /// The session this journal owns.
    fn session_id(&self) -> &SessionId;
}
