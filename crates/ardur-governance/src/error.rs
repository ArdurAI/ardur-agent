//! Error type for the governance seam.

use thiserror::Error;

/// Failures projecting, signing, verifying, or enforcing a governed action.
#[derive(Debug, Error)]
pub enum GovernanceError {
    /// A claim value violated an MCEP Execution Receipt v0.1 schema constraint
    /// (e.g. an `idString` outside 8..=64 chars or the run-nonce length bound).
    #[error("execution-receipt claim invalid: {0}")]
    InvalidClaim(String),

    /// The tri-state verdict / denial-field invariant was violated (a
    /// `compliant` receipt MUST NOT carry denial fields; a
    /// `violation`/`insufficient_evidence` receipt MUST carry both).
    #[error("verdict/denial invariant violated: {0}")]
    VerdictInvariant(String),

    /// JWS signing or serialization failed.
    #[error("execution-receipt signing failed: {0}")]
    Sign(String),

    /// JWS signature, header, or structural verification failed.
    #[error("execution-receipt verification failed: {0}")]
    Verify(String),

    /// The mirror-chain linkage (`parent_receipt_hash` / `parent_receipt_id`)
    /// did not match the preceding receipt.
    #[error("execution-receipt chain broken at index {index}: {detail}")]
    BrokenChain {
        /// Zero-based position of the first receipt whose linkage failed.
        index: usize,
        /// What mismatched.
        detail: String,
    },

    /// PKCS#8 PEM key custody failed.
    #[error("key custody failed: {0}")]
    Key(String),

    /// Kernel enforcement is unavailable on this platform/target (true BPF-LSM
    /// deny is Linux + managed-cgroup only). Carries the reason for audit.
    #[error("enforcement unavailable: {0}")]
    EnforcementUnavailable(String),
}
