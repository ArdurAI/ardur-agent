//! [`RecordingHook`] — a built-in observer hook that records every callback it
//! receives into a shared, ordered log, so tests can assert *what* fired, *in
//! what order*, and *with what context*.
//!
//! It is a pure observer: `on_pre_submit` always returns
//! [`HookDecision::Continue`], and the other callbacks return `Ok(())`. Several
//! `RecordingHook`s can share one log (clone the [`event_log`](RecordingHook::event_log)
//! handle) to assert cross-hook ordering.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::hook::{
    ErrorCtx, HookDecision, HookError, HookId, LifecycleHook, LifecyclePhase, PostReceiptCtx,
    PreSubmitCtx, RevokeCtx,
};
use ardur_runtime::SessionId;

/// One recorded lifecycle callback. Carries the `hook_id` that observed it so a
/// shared log across several hooks stays attributable.
#[derive(Clone, Debug, PartialEq)]
pub enum HookEvent {
    /// An `on_pre_submit` firing.
    OnPreSubmit {
        /// The hook that observed it.
        hook_id: HookId,
        /// The session of the turn.
        session_id: SessionId,
    },
    /// An `on_post_receipt` firing.
    OnPostReceipt {
        /// The hook that observed it.
        hook_id: HookId,
        /// The session of the turn.
        session_id: SessionId,
        /// The minted receipt's id.
        receipt_id: Uuid,
    },
    /// An `on_error` firing.
    OnError {
        /// The hook that observed it.
        hook_id: HookId,
        /// The session of the turn.
        session_id: SessionId,
        /// The phase the failure occurred at.
        phase: LifecyclePhase,
        /// The error's `Display` rendering, captured at observation time.
        message: String,
    },
    /// An `on_revoke` firing.
    OnRevoke {
        /// The hook that observed it.
        hook_id: HookId,
        /// The session of the turn.
        session_id: SessionId,
        /// The revocation reason.
        revocation_reason: String,
    },
}

/// A shared, cloneable handle to a [`RecordingHook`]'s ordered event log.
pub type EventLog = Arc<Mutex<Vec<HookEvent>>>;

/// A [`LifecycleHook`] that records every callback into a shared [`EventLog`].
pub struct RecordingHook {
    id: HookId,
    priority: i32,
    events: EventLog,
}

impl RecordingHook {
    /// Create a recording hook with its own fresh event log, priority `0`.
    pub fn new(id: impl Into<HookId>) -> Self {
        Self {
            id: id.into(),
            priority: 0,
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Create a recording hook writing into an existing shared log (so several
    /// hooks can record into one ordered log to assert cross-hook ordering).
    pub fn with_shared_log(id: impl Into<HookId>, priority: i32, events: EventLog) -> Self {
        Self {
            id: id.into(),
            priority,
            events,
        }
    }

    /// A clone of the shared event-log handle.
    #[must_use]
    pub fn event_log(&self) -> EventLog {
        Arc::clone(&self.events)
    }

    /// A snapshot copy of the events recorded so far, in order.
    #[must_use]
    pub fn events(&self) -> Vec<HookEvent> {
        self.events.lock().clone()
    }
}

#[async_trait]
impl LifecycleHook for RecordingHook {
    async fn on_pre_submit(&self, ctx: &PreSubmitCtx<'_>) -> HookDecision {
        self.events.lock().push(HookEvent::OnPreSubmit {
            hook_id: self.id.clone(),
            session_id: ctx.session_id,
        });
        HookDecision::Continue
    }

    async fn on_post_receipt(&self, ctx: &PostReceiptCtx<'_>) -> Result<(), HookError> {
        self.events.lock().push(HookEvent::OnPostReceipt {
            hook_id: self.id.clone(),
            session_id: ctx.session_id,
            receipt_id: ctx.receipt.receipt_id,
        });
        Ok(())
    }

    async fn on_error(&self, ctx: &ErrorCtx<'_>) -> Result<(), HookError> {
        self.events.lock().push(HookEvent::OnError {
            hook_id: self.id.clone(),
            session_id: ctx.session_id,
            phase: ctx.phase,
            message: ctx.error.to_string(),
        });
        Ok(())
    }

    async fn on_revoke(&self, ctx: &RevokeCtx<'_>) -> Result<(), HookError> {
        self.events.lock().push(HookEvent::OnRevoke {
            hook_id: self.id.clone(),
            session_id: ctx.session_id,
            revocation_reason: ctx.revocation_reason.clone(),
        });
        Ok(())
    }

    fn hook_id(&self) -> HookId {
        self.id.clone()
    }

    fn priority(&self) -> i32 {
        self.priority
    }
}
