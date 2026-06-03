//! The runtime's single typed-error surface.
//!
//! Every fallible operation in this crate returns [`RuntimeError`]. The
//! variants map onto the §1.0 admission/dispatch failure modes the caller must
//! distinguish: a missing token is not an expired one, and a cost rejection is
//! not a dead provider.

/// All ways a runtime or command-bus operation can fail.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// The request carried no capability token, or the referenced token could
    /// not be resolved.
    #[error("capability token missing or unresolved")]
    CapTokenMissing,

    /// The referenced capability token has expired and can no longer authorize
    /// a turn.
    #[error("capability token expired")]
    CapTokenExpired,

    /// The capability token was rejected at verification — a forged or malformed
    /// token, a revoked token, an audience/tool mismatch, or an exhausted budget.
    /// Carries the verifier's reason. Distinct from [`CapTokenMissing`] (no token
    /// at all) and [`CapTokenExpired`] (a once-valid token past its expiry): this
    /// is a present token the authority *declined*. See §11.14 (`ardur-cap-token`)
    /// and the Phase-2 fused runtime, which verifies the token before any other
    /// stage runs.
    ///
    /// [`CapTokenMissing`]: RuntimeError::CapTokenMissing
    /// [`CapTokenExpired`]: RuntimeError::CapTokenExpired
    #[error("capability token denied: {reason}")]
    CapDenied {
        /// The human-readable reason the verifier gave for the denial.
        reason: String,
    },

    /// A policy decision denied the turn. Carries the deciding engine's reason.
    /// See §11.0 (`ardur-cedar-policy`): the fused runtime evaluates the turn
    /// against the Cedar bundle after the cap-token verifies but before any
    /// budget is reserved, so a `Deny` (or `Indeterminate`) surfaces here.
    #[error("policy denied: {reason}")]
    PolicyDenied {
        /// The human-readable reason the policy engine gave for the denial.
        reason: String,
    },

    /// Admitting the turn would exceed the session's configured cost ceiling.
    #[error("cost ceiling exceeded")]
    CostCeilingExceeded,

    /// The requested provider is not registered or is currently unreachable.
    #[error("provider unavailable")]
    ProviderUnavailable,

    /// A lifecycle hook vetoed the turn before it reached the provider. Carries
    /// the id of the hook that blocked and the reason it gave. See §11.17
    /// (`ardur-lifecycle-hooks`): a pre-submit hook returning `Veto` aborts the
    /// submit, and the registry's first-veto-wins composition names the
    /// blocking hook here so the caller can surface *which* policy fired.
    #[error("turn vetoed by lifecycle hook `{hook_id}`: {reason}")]
    VetoedByHook {
        /// The `hook_id` of the hook that vetoed the turn.
        hook_id: String,
        /// The human-readable reason the hook gave for blocking.
        reason: String,
    },

    /// Provisioning a per-request budget failed before the turn could be
    /// admitted — e.g. an additive top-up would breach the gate's configured
    /// per-subject cap. Carries the subject the top-up targeted and the reason.
    /// See `ardur-fused-runtime`'s `submit_with_provisioning`, which provisions a
    /// holder's budget on the request itself before the cost gate reserves
    /// against it. Distinct from [`CostCeilingExceeded`] (the holder *has* a
    /// budget but it cannot cover the turn): this is the top-up itself being
    /// refused.
    ///
    /// [`CostCeilingExceeded`]: RuntimeError::CostCeilingExceeded
    #[error("provisioning failed for `{subject}`: {reason}")]
    ProvisioningFailed {
        /// The subject the per-request top-up targeted.
        subject: String,
        /// The human-readable reason provisioning was refused.
        reason: String,
    },

    /// No command was registered under the dispatched name.
    #[error("command not found: {0}")]
    CommandNotFound(String),

    /// An otherwise-unclassified internal failure.
    #[error("internal runtime error: {0}")]
    Internal(#[from] anyhow::Error),
}
