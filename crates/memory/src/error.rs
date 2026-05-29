//! The crate's single typed-error surface.
//!
//! Every fallible operation in `ardur-memory` returns [`MemoryError`]. Reads
//! (`at_time`, `current_as_of`, `history_of`) are infallible by construction —
//! they return an empty `Vec` rather than an error — so only the append-side
//! operations (`record`, `invalidate`) surface this type.

use uuid::Uuid;

/// A convenience alias so the trait surface reads `Result<RecordId>` rather
/// than `Result<RecordId, MemoryError>`.
pub type Result<T> = std::result::Result<T, MemoryError>;

/// All ways a memory operation can fail.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    /// `invalidate` (or any id-addressed operation) referenced a `record_id`
    /// that is not present in the store.
    #[error("no memory record with id `{0}`")]
    NotFound(Uuid),

    /// The backing lock was poisoned by a panic in another thread. The Phase 1
    /// `parking_lot` store never poisons, so this is reserved for the Phase 2
    /// persistence backend — see `// TODO §7.0 Phase 2` in `runtime.rs`.
    #[error("memory store lock was poisoned")]
    LockPoisoned,

    /// A record could not be interpreted — e.g. a payload that violates a
    /// kind-specific shape invariant. Carries a human-readable reason.
    #[error("malformed memory record: {0}")]
    Malformed(String),
}
