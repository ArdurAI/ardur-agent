//! The runtime's single typed-error surface.
//!
//! Every fallible operation in this crate returns [`RuntimeError`]. The
//! variants map onto the §1.0 admission/dispatch failure modes the caller must
//! distinguish: a missing token is not an expired one, and a cost rejection is
//! not a dead provider.
//!
//! # The injection signal types live here (ARD-48)
//!
//! [`InjectionFlag`] and [`FlagCategory`] are *hoisted* into this module from
//! the §11.16 injection-defense crate so [`RuntimeError::InjectionBlocked`] can
//! carry the flags that justified a block without `ardur-runtime` depending on
//! `ardur-injection-defense`. That dependency would be a **cycle**:
//! injection-defense already depends (transitively, via `ardur-tool-registry`
//! and `ardur-messaging-gateway`) on this crate, so the error surface that names
//! its flags must own the flag types, and injection-defense re-exports them via
//! `pub use ardur_runtime::{FlagCategory, InjectionFlag}`. See §18.8.

use serde::{Deserialize, Serialize};

/// The class of injection a flag belongs to. A single scan can raise flags
/// across several categories.
///
/// Owned here (rather than in `ardur-injection-defense`) so
/// [`RuntimeError::InjectionBlocked`] can name it; injection-defense re-exports
/// it. See the module docs for the dependency-cycle rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FlagCategory {
    /// Attempts to override or discard prior/system instructions
    /// (e.g. "ignore all previous instructions").
    InstructionOverride,
    /// Attempts to reassign the model's role or persona
    /// (e.g. "you are now a …", "pretend to be …").
    RoleHijack,
    /// Abuse of chat/template delimiters or role markers
    /// (e.g. `<|im_start|>`, `[[INST]]`, `</system>`).
    DelimiterAbuse,
    /// Attempts to extract secrets or sensitive data
    /// (e.g. "exfiltrate the api key", "print my password").
    DataExfiltration,
    /// Known jailbreak invocations (e.g. "DAN mode", "do anything now").
    JailbreakAttempt,
}

/// A single pattern match raised during an injection-defense scan.
///
/// Owned here (rather than in `ardur-injection-defense`) so
/// [`RuntimeError::InjectionBlocked`] can carry it; injection-defense re-exports
/// it. See the module docs for the dependency-cycle rationale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InjectionFlag {
    /// Stable identifier of the pattern that matched
    /// (e.g. `"ignore_previous_instructions"`).
    pub pattern_id: String,
    /// The exact substring of the scanned content that matched.
    pub matched_text: String,
    /// How strongly this match indicates an injection attempt, in `0.0..=1.0`.
    pub confidence: f32,
    /// The injection class this match belongs to.
    pub category: FlagCategory,
}

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

    /// The injection-defense layer blocked the turn before it reached the
    /// provider. Carries the id of the filter stage that blocked, the human
    /// reason (the matched signatures), and every [`InjectionFlag`] the scan
    /// raised. See §11.16 (`ardur-injection-defense`) and the fused runtime's
    /// stage 4.5, which scans the outbound prompt after the pre-submit hooks but
    /// before the provider dispatch: a `Block` verdict releases the cost
    /// reservation, fires `on_error`, and surfaces here — so the provider, and
    /// every billing/receipt side effect downstream of it, never runs.
    ///
    /// `#[non_exhaustive]`: future scans may attach more context (e.g. the
    /// content source, or a per-filter breakdown) without a breaking change, so
    /// downstream `match` arms must use `..`. Construct it through
    /// [`RuntimeError::injection_blocked`] (the struct literal is crate-private
    /// under `#[non_exhaustive]`).
    #[error("injection blocked by filter `{filter_id}`: {reason}")]
    #[non_exhaustive]
    InjectionBlocked {
        /// The id of the injection-defense stage that produced the block.
        filter_id: String,
        /// The human-readable reason — the matched injection signatures.
        reason: String,
        /// Every flag the scan raised (the union across all filters).
        flags: Vec<InjectionFlag>,
    },

    /// No command was registered under the dispatched name.
    #[error("command not found: {0}")]
    CommandNotFound(String),

    /// An otherwise-unclassified internal failure.
    #[error("internal runtime error: {0}")]
    Internal(#[from] anyhow::Error),
}

impl RuntimeError {
    /// Construct an [`InjectionBlocked`](RuntimeError::InjectionBlocked). Because
    /// that variant is `#[non_exhaustive]`, out-of-crate callers (the fused
    /// runtime's stage 4.5) cannot use the struct literal, so this is the
    /// supported constructor.
    pub fn injection_blocked(
        filter_id: impl Into<String>,
        reason: impl Into<String>,
        flags: Vec<InjectionFlag>,
    ) -> Self {
        RuntimeError::InjectionBlocked {
            filter_id: filter_id.into(),
            reason: reason.into(),
            flags,
        }
    }
}
