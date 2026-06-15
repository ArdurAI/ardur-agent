//! The hook contract: the [`LifecycleHook`] trait, the per-event context
//! views, the [`HookDecision`] a pre-submit hook returns, and the supporting
//! id / phase / error types.
//!
//! A hook observes — and, at pre-submit, may veto or rewrite — the lifecycle of
//! a single turn as it flows through [`HookedRuntime`](crate::HookedRuntime).
//! Phase 1 wires four moments:
//!
//! - [`LifecycleHook::on_pre_submit`] — before the request reaches the provider.
//!   The only veto/rewrite point: a hook may [`HookDecision::Veto`] the turn or
//!   [`HookDecision::Replace`] its request.
//! - [`LifecycleHook::on_post_receipt`] — after the receipt is minted, before
//!   the memory write. Pure observation; the turn already happened, so it cannot
//!   veto.
//! - [`LifecycleHook::on_error`] — on any error from the provider, cost gate, or
//!   an admission failure ([`LifecyclePhase`] names which).
//! - [`LifecycleHook::on_revoke`] — when a cap-token is revoked mid-session.
//!
//! The pre-veto / observe-after split mirrors ADR-Phase3-578: a hook may stop an
//! action before it happens, but the post-action moments are observational.

use ardur_provider_runtime::{CompletionRequest, CompletionResponse};
use ardur_receipt::{ReceiptBody, SignedReceipt};
use ardur_runtime::{CapTokenRef, CostTuple, SessionId};
use async_trait::async_trait;

/// Unique, stable identifier of a registered hook. A thin newtype over a
/// `String` so two hooks can be told apart in receipts, ordering, and the
/// [`RuntimeError::VetoedByHook`](ardur_runtime::RuntimeError::VetoedByHook)
/// a veto surfaces.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HookId(pub String);

impl HookId {
    /// Wrap a hook-id string.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the underlying id string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HookId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for HookId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for HookId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// The lifecycle moment an [`ErrorCtx`] reports a failure from. Lets an
/// `on_error` hook branch on *where* the turn broke without parsing the error
/// type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LifecyclePhase {
    /// Request admission, before the provider call (e.g. a missing cap-token).
    Submit,
    /// The provider completion call itself.
    Provider,
    /// Minting the turn's receipt.
    Receipt,
    /// Writing the turn record to memory.
    MemoryWrite,
    /// Appending to the session journal.
    JournalAppend,
}

/// What a pre-submit hook decides about a turn.
///
/// Returned by [`LifecycleHook::on_pre_submit`]; composed across all registered
/// pre-submit hooks by
/// [`HookRegistry::run_pre_submit`](crate::HookRegistry::run_pre_submit) under a
/// first-veto-wins, replacements-chain rule.
#[derive(Clone, Debug)]
pub enum HookDecision {
    /// Proceed unchanged.
    Continue,
    /// Abort the submit before the provider is called. Surfaces as
    /// [`RuntimeError::VetoedByHook`](ardur_runtime::RuntimeError::VetoedByHook).
    Veto {
        /// Human-readable reason the turn was blocked.
        reason: String,
    },
    /// Substitute the request body for everything downstream (use case:
    /// redaction, augmentation). Later pre-submit hooks see the replacement.
    Replace {
        /// The request to use in place of the one the hook was shown.
        new_request: CompletionRequest,
    },
}

/// All ways a hook callback can fail.
///
/// `on_post_receipt` / `on_error` / `on_revoke` return `Result<(), HookError>`;
/// the registry collects errors across the chain rather than short-circuiting,
/// because these moments are observational — one hook's failure must not hide
/// another's callback.
#[derive(Debug, thiserror::Error)]
pub enum HookError {
    /// An otherwise-unclassified internal failure inside the hook.
    #[error("internal hook error: {0}")]
    Internal(#[from] anyhow::Error),

    /// The hook exceeded its deadline.
    #[error("hook timed out")]
    Timeout,

    /// A hook-specific failure carrying its own message.
    #[error("hook error: {0}")]
    Custom(String),
}

/// The immutable view a pre-submit hook is shown of a turn about to be
/// dispatched. The `request` is the *current* request — for a hook that runs
/// after an earlier [`HookDecision::Replace`], it is the replaced request, not
/// the original.
#[derive(Debug)]
pub struct PreSubmitCtx<'a> {
    /// The session the turn belongs to.
    pub session_id: SessionId,
    /// The completion request about to be dispatched (or its current
    /// replacement).
    pub request: &'a CompletionRequest,
    /// The capability token authorizing the turn.
    pub cap_token_id: &'a CapTokenRef,
    /// 1-based attempt counter (a retried turn increments it). Phase 1 always
    /// dispatches with `attempt = 1`.
    pub attempt: u32,
}

/// The view a post-receipt hook is shown after the turn's receipt is minted,
/// before the memory write. Observational only — the turn has happened.
#[derive(Debug)]
pub struct PostReceiptCtx<'a> {
    /// The session the turn belongs to.
    pub session_id: SessionId,
    /// The signed receipt envelope minted for the turn. Its compact JWS is the
    /// canonical byte sequence used for ES256 verification and receipt-chain
    /// hashing.
    pub signed_receipt: &'a SignedReceipt,
    /// The decoded receipt body carried by [`signed_receipt`](Self::signed_receipt).
    /// Its `payload_digest` covers the *response actually produced* — i.e. the
    /// post-redaction text when a pre-submit hook rewrote the request.
    pub receipt: &'a ReceiptBody,
    /// The provider's completion response.
    pub response: &'a CompletionResponse,
    /// The cost charged for the turn.
    pub cost: CostTuple,
}

/// The view an error hook is shown when a turn fails. `phase` names where the
/// failure occurred; `error` is the underlying error as a trait object so the
/// context stays decoupled from any one crate's error type.
///
/// The trait object is `Send + Sync` (not a bare `dyn Error`) so an `ErrorCtx`
/// is itself `Send + Sync` — the property that lets every hook future, and the
/// registry/runtime futures that await them, be `Send` and so run on a
/// work-stealing executor (e.g. an axum handler) without a current-thread
/// bridge. Every runtime error this context carries (`RuntimeError`,
/// `ProviderError`, `io::Error`, the memory/journal errors) is already
/// `Send + Sync`, so the tighter bound costs callers nothing.
pub struct ErrorCtx<'a> {
    /// The session the turn belonged to.
    pub session_id: SessionId,
    /// Which lifecycle moment the failure occurred at.
    pub phase: LifecyclePhase,
    /// The underlying error.
    pub error: &'a (dyn std::error::Error + Send + Sync + 'a),
}

impl std::fmt::Debug for ErrorCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ErrorCtx")
            .field("session_id", &self.session_id)
            .field("phase", &self.phase)
            .field("error", &self.error.to_string())
            .finish()
    }
}

/// The view a revoke hook is shown when a cap-token is revoked mid-session.
#[derive(Debug)]
pub struct RevokeCtx<'a> {
    /// The session whose authorizing token was revoked.
    pub session_id: SessionId,
    /// The token that was revoked.
    pub cap_token_id: &'a CapTokenRef,
    /// Why the token was revoked.
    pub revocation_reason: String,
}

/// A unit of policy wired into the turn lifecycle.
///
/// Object-safe (via `async-trait`): the [`HookRegistry`](crate::HookRegistry)
/// holds hooks as `Arc<dyn LifecycleHook>`. Every callback has a no-op default
/// so a hook implements only the moments it cares about; `hook_id` is the one
/// required method.
///
/// The trait is `Send + Sync` (a hook is shareable across threads) and its async
/// callbacks return **`Send`** futures (plain `#[async_trait]`). That `Send`
/// bound is what lets a [`FusedRuntime`](../ardur_fused_runtime/struct.FusedRuntime.html)
/// `submit` future — which awaits the hook registry — itself be `Send`, so it
/// runs directly on a work-stealing executor (an axum handler) rather than
/// behind a current-thread worker bridge. It is affordable because [`ErrorCtx`]
/// now carries a `Send + Sync` error (the only context that held a bare
/// `dyn Error`), and every other context view is already `Send + Sync`.
#[async_trait]
pub trait LifecycleHook: Send + Sync {
    /// Called before the runtime forwards the request to the provider. A
    /// [`HookDecision::Veto`] blocks the turn; a [`HookDecision::Replace`] swaps
    /// the request for everything downstream. Defaults to
    /// [`HookDecision::Continue`].
    async fn on_pre_submit(&self, ctx: &PreSubmitCtx<'_>) -> HookDecision {
        let _ = ctx;
        HookDecision::Continue
    }

    /// Called after the receipt is minted, before the memory write. Cannot veto
    /// — the turn already happened. Defaults to `Ok(())`.
    async fn on_post_receipt(&self, ctx: &PostReceiptCtx<'_>) -> Result<(), HookError> {
        let _ = ctx;
        Ok(())
    }

    /// Called on any error from the provider, cost gate, or an admission /
    /// memory failure. Defaults to `Ok(())`.
    async fn on_error(&self, ctx: &ErrorCtx<'_>) -> Result<(), HookError> {
        let _ = ctx;
        Ok(())
    }

    /// Called when a cap-token is revoked mid-session. Defaults to `Ok(())`.
    async fn on_revoke(&self, ctx: &RevokeCtx<'_>) -> Result<(), HookError> {
        let _ = ctx;
        Ok(())
    }

    /// This hook's unique id. Required.
    fn hook_id(&self) -> HookId;

    /// Composition priority: **lower runs first**. Defaults to `0`. Hooks with
    /// equal priority keep registration order (the sort is stable).
    fn priority(&self) -> i32 {
        0
    }
}
