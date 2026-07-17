//! The crate's typed-error surfaces: [`JournalError`] for an append/replay that
//! fails, and [`RegistryError`] for a registration that is rejected.

use crate::types::EntryId;
use ardur_runtime::SessionId;

/// Every way a [`SessionJournal`](crate::SessionJournal) operation can fail.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// A replay named a session this journal does not own.
    #[error("no journal for session `{0:?}`")]
    SessionNotFound(SessionId),

    /// A cursor named an entry past the end of the log.
    #[error("no entry at position `{0}`")]
    EntryNotFound(EntryId),

    /// An I/O failure reading or writing the backing file.
    #[error("journal i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// A serialized entry could not be encoded or a stored line decoded.
    #[error("journal serde error: {0}")]
    Serde(#[from] serde_json::Error),

    /// A guarded resource was poisoned by a panic in another holder. The
    /// non-poisoning `parking_lot` primitives this crate uses never produce
    /// this; it exists so a future poisoning backend has a variant to map onto.
    #[error("journal lock poisoned")]
    LockPoisoned,

    /// An append arrived with a position behind the log's current head — the
    /// monotonic-id invariant would be violated.
    #[error("journal entry out of order")]
    EntryOutOfOrder,

    /// A stored line was not a well-formed [`JournalEntry`](crate::JournalEntry),
    /// or a value failed a structural check (e.g. a malformed digest).
    #[error("malformed journal data: {0}")]
    Malformed(String),
}

/// Every way a [`JournalRegistry::register`](crate::JournalRegistry::register)
/// call can be rejected.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// A journal for this session is already registered. Registration is keyed
    /// by [`SessionId`]; the registry refuses to silently replace an existing
    /// journal. Use [`get_or_create`](crate::JournalRegistry::get_or_create)
    /// for the resolve-or-mint path.
    #[error("a journal is already registered for session `{0:?}`")]
    AlreadyRegistered(SessionId),
}
