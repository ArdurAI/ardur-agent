//! The [`HookRegistry`]: an ordered set of [`LifecycleHook`]s and the
//! composition rules that turn N per-hook decisions into one outcome the
//! runtime honours.
//!
//! Composition (ADR-Phase3-578, Phase 1 subset):
//! - **Pre-submit** runs hooks in priority order; the **first `Veto` wins** and
//!   short-circuits the rest; a `Replace` **chains** (each later hook sees the
//!   prior hook's replacement); absent any veto/replace the outcome is
//!   `Continue`. The composed outcome ([`PreSubmitOutcome`]) carries the
//!   *provenance* a bare [`HookDecision`] lacks — which hook vetoed — so the
//!   runtime can name it in
//!   [`RuntimeError::VetoedByHook`](ardur_runtime::RuntimeError::VetoedByHook).
//! - **Post-receipt / error / revoke** run **all** hooks and **collect** their
//!   errors without short-circuiting — these moments are observational, so one
//!   hook's failure must not suppress another's callback.

use std::sync::Arc;

use crate::hook::{
    ErrorCtx, HookDecision, HookError, HookId, LifecycleHook, PostReceiptCtx, PreSubmitCtx,
    RevokeCtx,
};

/// The composed result of running every registered pre-submit hook.
///
/// Distinct from [`HookDecision`] (what a single hook returns) because the
/// composed outcome must record *which* hook vetoed — provenance a per-hook
/// decision does not carry. Mirrors ADR-Phase3-578's `HookCompositionOutcome`.
#[derive(Clone, Debug)]
pub enum PreSubmitOutcome {
    /// No hook vetoed or replaced; proceed with the original request.
    Continue,
    /// A hook vetoed the turn. Carries the blocking hook's id and its reason.
    Vetoed {
        /// The hook that vetoed (the first, in priority order).
        hook_id: HookId,
        /// The reason it gave.
        reason: String,
    },
    /// One or more hooks replaced the request. Carries the final, fully-chained
    /// request to dispatch.
    Replaced {
        /// The request to dispatch in place of the original.
        request: ardur_provider_runtime::CompletionRequest,
    },
}

/// An ordered registry of [`LifecycleHook`]s, kept sorted by ascending
/// priority (lower runs first; ties keep registration order).
#[derive(Default)]
pub struct HookRegistry {
    hooks: Vec<Arc<dyn LifecycleHook>>,
}

impl HookRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a hook, keeping the registry sorted by ascending priority. The
    /// sort is stable, so hooks with equal priority fire in registration order.
    pub fn register(&mut self, hook: Arc<dyn LifecycleHook>) {
        self.hooks.push(hook);
        self.hooks.sort_by_key(|h| h.priority());
    }

    /// The number of registered hooks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// Whether no hooks are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Run every pre-submit hook in priority order. First `Veto` wins and
    /// short-circuits; `Replace` chains through later hooks; otherwise
    /// `Continue`.
    pub async fn run_pre_submit(&self, ctx: &PreSubmitCtx<'_>) -> PreSubmitOutcome {
        // The request the *next* hook sees: `None` until a Replace happens,
        // then the chained replacement. We rebuild the per-hook context against
        // this evolving request so a redaction hook's output feeds the next.
        let mut replaced: Option<ardur_provider_runtime::CompletionRequest> = None;

        for hook in &self.hooks {
            let current = replaced.as_ref().unwrap_or(ctx.request);
            let hook_ctx = PreSubmitCtx {
                session_id: ctx.session_id,
                request: current,
                cap_token_id: ctx.cap_token_id,
                attempt: ctx.attempt,
            };
            match hook.on_pre_submit(&hook_ctx).await {
                HookDecision::Continue => {}
                HookDecision::Veto { reason } => {
                    return PreSubmitOutcome::Vetoed {
                        hook_id: hook.hook_id(),
                        reason,
                    };
                }
                HookDecision::Replace { new_request } => {
                    replaced = Some(new_request);
                }
            }
        }

        match replaced {
            Some(request) => PreSubmitOutcome::Replaced { request },
            None => PreSubmitOutcome::Continue,
        }
    }

    /// Run every post-receipt hook, collecting (not short-circuiting) errors.
    pub async fn run_post_receipt(&self, ctx: &PostReceiptCtx<'_>) -> Vec<HookError> {
        let mut errors = Vec::new();
        for hook in &self.hooks {
            if let Err(e) = hook.on_post_receipt(ctx).await {
                errors.push(e);
            }
        }
        errors
    }

    /// Run every error hook, collecting (not short-circuiting) errors.
    pub async fn run_error(&self, ctx: &ErrorCtx<'_>) -> Vec<HookError> {
        let mut errors = Vec::new();
        for hook in &self.hooks {
            if let Err(e) = hook.on_error(ctx).await {
                errors.push(e);
            }
        }
        errors
    }

    /// Run every revoke hook, collecting (not short-circuiting) errors.
    pub async fn run_revoke(&self, ctx: &RevokeCtx<'_>) -> Vec<HookError> {
        let mut errors = Vec::new();
        for hook in &self.hooks {
            if let Err(e) = hook.on_revoke(ctx).await {
                errors.push(e);
            }
        }
        errors
    }
}
