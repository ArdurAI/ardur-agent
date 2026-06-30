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

    /// The verified cap-token claims did not carry the required memory
    /// capability for this high-level operation.
    #[error("memory capability denied for {action:?}: requires {required}")]
    CapabilityDenied {
        /// Operation being attempted.
        action: crate::MemoryAction,
        /// Capability string that was required.
        required: String,
    },

    /// Cedar denied or could not safely decide a memory operation.
    #[error("memory policy denied for {action:?}: {reason}")]
    PolicyDenied {
        /// Operation being attempted.
        action: crate::MemoryAction,
        /// Cedar denial or indeterminate reason.
        reason: String,
    },

    /// The cap-token subject did not match the memory subject/scope.
    #[error(
        "memory subject mismatch: cap-token subject `{claim_subject}` cannot access `{memory_subject}`"
    )]
    SubjectMismatch {
        /// Subject from the verified cap-token claims.
        claim_subject: String,
        /// Subject on the requested memory operation.
        memory_subject: String,
    },

    /// A memory write was attempted without a receipt id to chain it to the
    /// audit log.
    #[error("memory operation {action:?} requires a receipt id")]
    ReceiptRequired {
        /// Operation being attempted.
        action: crate::MemoryAction,
    },

    /// A persistence backend (the §7.0 Phase 2 durable stores — e.g. the
    /// Qdrant-backed `ardur-memory-qdrant`) failed to complete an operation:
    /// a transport error, a collection that could not be created, or a payload
    /// that did not round-trip. Carries a human-readable reason. The in-process
    /// [`crate::InMemoryMemoryRuntime`] never returns this.
    #[error("memory backend error: {0}")]
    Backend(String),
}
