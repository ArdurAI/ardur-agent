//! Supervised provider-backed child agents for `delegate_task` (gh#490).
//!
//! `delegate_task` has always spawned an *in-memory* child: useful for testing
//! the delegation shape, but it never proved the properties that make
//! delegation safe when the child is a real, cost-incurring, cancellable
//! process. This crate supplies the missing half — a supervised child that runs
//! on a real [`Provider`] (the prime-agent RPC executor from gh#501/#505) under
//! an attenuated capability token and a *reserved share* of the parent's
//! budget.
//!
//! It is deliberately standalone. The integration point,
//! `crates/fused-runtime/src/runtime.rs`, is owned by an in-flight workstream
//! (E4.3, gh#496), so wiring happens in a follow-up PR; everything here is
//! exercised through its own public API in the meantime.
//!
//! # The invariants this crate exists to hold
//!
//! **Cancellation follows the worker, not the handle.** Dropping a
//! [`JoinHandle`](tokio::task::JoinHandle) detaches a task; it does not stop it.
//! A child that keeps talking to a provider after its parent believes it dead
//! is still spending money and still holding authority. [`ChildHandle::cancel`]
//! therefore returns only once the worker has actually observed the stop and
//! run its cleanup — see [`ChildHandle::cancel`] for the precise ordering.
//!
//! **Revocation is checked at action boundaries.** A capability is not a
//! one-time gate at spawn: it is re-checked immediately before *every* provider
//! dispatch. A token revoked mid-turn stops the next round rather than being
//! noticed only when the child finishes.
//!
//! **A cancelled child still settles.** Cancellation produces a terminal
//! receipt carrying the cost actually incurred, following the
//! `llm.completion.cancelled.v1` precedent already established in
//! `fused-runtime`. Silence would leave the parent's reservation dangling and
//! the spend unaccounted.
//!
//! **Typed denials keep their origin.** A child denied by policy, by a revoked
//! token, or by budget exhaustion produces three *different* outcomes. Flattening
//! them into "child failed" destroys the operator's ability to tell a
//! revocation from a bug.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use ardur_cap_token::{CapToken, DenyList};
use ardur_core_types::{CostEnvelope, CostTuple, ModelId};
use ardur_provider_runtime::{ChatMessage, CompletionRequest, Provider, ProviderError};
use thiserror::Error;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

/// The receipt verb recorded when a child is cancelled before completing.
///
/// Mirrors `fused-runtime`'s `llm.completion.cancelled.v1`: a cancelled unit of
/// work is a *terminal, settled* outcome, not an absence of one.
pub const CHILD_CANCELLED_VERB: &str = "delegate.child.cancelled.v1";

/// The receipt verb recorded when a child runs to completion.
pub const CHILD_COMPLETED_VERB: &str = "delegate.child.completed.v1";

/// Why a child stopped.
///
/// Every variant is a *terminal* state that settles: the distinction exists so
/// an operator can tell a deliberate revocation from an exhausted budget from a
/// provider fault. Collapsing them would make a revocation indistinguishable
/// from a crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildOutcome {
    /// Ran to completion and produced a final assistant message.
    Completed {
        /// The child's final text.
        text: String,
        /// Rounds actually dispatched to the provider.
        rounds: u32,
    },
    /// Stopped because the caller asked it to stop.
    Cancelled {
        /// Rounds completed before the stop was observed.
        rounds: u32,
    },
    /// Stopped because its capability token was revoked.
    ///
    /// Distinct from [`Cancelled`](Self::Cancelled): the parent did not ask for
    /// this, the authority was withdrawn underneath it.
    Revoked {
        /// Rounds completed before the revocation was observed.
        rounds: u32,
    },
    /// Stopped because the next round could not fit in the reserved budget.
    ///
    /// The child is *not* at fault and the work is not broken — it ran out of
    /// the allowance it was given, which is a normal, expected outcome.
    BudgetExhausted {
        /// Rounds completed before the budget ran out.
        rounds: u32,
    },
    /// The provider failed and the failure is preserved, not flattened.
    Failed {
        /// The provider's own error rendering.
        reason: String,
        /// Rounds completed before the failure.
        rounds: u32,
    },
}

impl ChildOutcome {
    /// The receipt verb this outcome settles under.
    ///
    /// Only a clean completion is `completed`; every other terminal state —
    /// including budget exhaustion, which is nobody's fault — settles as
    /// cancelled, because the work did not finish.
    #[must_use]
    pub fn verb(&self) -> &'static str {
        match self {
            Self::Completed { .. } => CHILD_COMPLETED_VERB,
            Self::Cancelled { .. }
            | Self::Revoked { .. }
            | Self::BudgetExhausted { .. }
            | Self::Failed { .. } => CHILD_CANCELLED_VERB,
        }
    }

    /// Rounds actually dispatched before stopping.
    #[must_use]
    pub fn rounds(&self) -> u32 {
        match self {
            Self::Completed { rounds, .. }
            | Self::Cancelled { rounds }
            | Self::Revoked { rounds }
            | Self::BudgetExhausted { rounds }
            | Self::Failed { rounds, .. } => *rounds,
        }
    }

    /// Whether this outcome represents work that finished as intended.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }
}

/// Errors raised while *starting* a child.
///
/// Distinct from [`ChildOutcome`], which describes a child that started and
/// then stopped. A spawn failure never produced a worker, so it never settles.
#[derive(Debug, Error)]
pub enum ChildError {
    /// The requested reservation exceeds what the parent holds.
    #[error("child reservation of {requested} cents exceeds the parent's {available} available")]
    ReservationTooLarge {
        /// Cents the caller asked to reserve.
        requested: u64,
        /// Cents the parent actually has left.
        available: u64,
    },
    /// The token was already revoked before the child started.
    #[error("cap-token was revoked before the child started")]
    AlreadyRevoked,
    /// The prompt was empty, so there is nothing to delegate.
    #[error("refusing to spawn a child with an empty prompt")]
    EmptyPrompt,
    /// The worker task itself panicked or was aborted out from under us.
    #[error("child worker terminated abnormally: {0}")]
    WorkerLost(String),
}

/// What a child is allowed to spend, carved out of its parent's allowance.
///
/// This is a *reservation*, not a limit: the cents are held against the parent
/// for the child's lifetime and released at settlement. Two children cannot
/// both spend the same cent.
#[derive(Debug, Clone)]
pub struct BudgetReservation {
    reserved_cents: u64,
    spent_cents: Arc<AtomicU64>,
}

impl BudgetReservation {
    /// Reserve `cents` out of `available`.
    ///
    /// # Errors
    /// [`ChildError::ReservationTooLarge`] when the request exceeds what the
    /// parent holds — the check that stops a child from overspending its parent.
    pub fn reserve(cents: u64, available: u64) -> Result<Self, ChildError> {
        if cents > available {
            return Err(ChildError::ReservationTooLarge {
                requested: cents,
                available,
            });
        }
        Ok(Self {
            reserved_cents: cents,
            spent_cents: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Cents reserved for this child.
    #[must_use]
    pub fn reserved_cents(&self) -> u64 {
        self.reserved_cents
    }

    /// Cents spent so far.
    #[must_use]
    pub fn spent_cents(&self) -> u64 {
        self.spent_cents.load(Ordering::SeqCst)
    }

    /// Cents still available to this child.
    #[must_use]
    pub fn remaining_cents(&self) -> u64 {
        self.reserved_cents.saturating_sub(self.spent_cents())
    }

    /// Whether `cost` fits in what remains.
    ///
    /// Checked *before* dispatch: a round that cannot be paid for is never sent,
    /// so the child cannot overrun its reservation and then settle the overage.
    #[must_use]
    pub fn can_afford(&self, cost: &CostTuple) -> bool {
        self.remaining_cents() >= cost.cents
    }

    /// Record spend against the reservation, saturating at the reserved total.
    pub fn record_spend(&self, cost: &CostTuple) {
        self.spent_cents.fetch_add(cost.cents, Ordering::SeqCst);
    }
}

/// Configuration for one supervised child.
pub struct ChildSpec {
    /// The prompt handed to the child.
    pub prompt: String,
    /// The child's attenuated capability token.
    pub token: CapToken,
    /// The child's reserved budget share.
    pub reservation: BudgetReservation,
    /// The model the child runs on.
    pub model: ModelId,
    /// Maximum provider rounds before the child stops on its own.
    pub max_rounds: u32,
    /// Per-round cost envelope.
    pub envelope: CostEnvelope,
}

/// A running child, and the only supported way to stop one.
pub struct ChildHandle {
    cancel_tx: Option<oneshot::Sender<()>>,
    worker: JoinHandle<ChildOutcome>,
    cancelled: Arc<AtomicBool>,
}

impl ChildHandle {
    /// Whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelling(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Ask the child to stop and **wait until it actually has**.
    ///
    /// The ordering matters and is the whole point of this type:
    ///
    /// 1. set the cancel flag, so a worker between rounds sees it;
    /// 2. fire the cancel channel, so a worker *inside* a provider call is
    ///    interrupted at its `select!`;
    /// 3. await the worker's join handle.
    ///
    /// Step 3 is what distinguishes real cancellation from the bug this crate
    /// exists to avoid. Returning after step 2 would report "cancelled" while
    /// the child was still mid-dispatch, still spending, and still holding its
    /// capability — exactly the orphaned-work failure seen in gh#422. The
    /// returned [`ChildOutcome`] is the worker's own terminal state, so a child
    /// that completed just as the cancel arrived truthfully reports
    /// [`Completed`](ChildOutcome::Completed) rather than a cancellation that
    /// did not happen.
    ///
    /// # Errors
    /// [`ChildError::WorkerLost`] if the worker panicked; its terminal state is
    /// then genuinely unknown and must not be invented.
    pub async fn cancel(mut self) -> Result<ChildOutcome, ChildError> {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(tx) = self.cancel_tx.take() {
            // A closed receiver means the worker already finished - not an error.
            let _ = tx.send(());
        }
        self.worker
            .await
            .map_err(|e| ChildError::WorkerLost(e.to_string()))
    }

    /// Wait for the child to finish on its own.
    ///
    /// # Errors
    /// [`ChildError::WorkerLost`] if the worker panicked.
    pub async fn join(self) -> Result<ChildOutcome, ChildError> {
        self.worker
            .await
            .map_err(|e| ChildError::WorkerLost(e.to_string()))
    }
}

/// Spawns supervised children onto a real provider.
pub struct ChildSupervisor<P: Provider + 'static> {
    provider: Arc<P>,
    deny_list: Arc<Mutex<Box<dyn DenyList + Send>>>,
}

impl<P: Provider + 'static> ChildSupervisor<P> {
    /// Build a supervisor over `provider`, consulting `deny_list` for revocation.
    pub fn new(provider: Arc<P>, deny_list: Box<dyn DenyList + Send>) -> Self {
        Self {
            provider,
            deny_list: Arc::new(Mutex::new(deny_list)),
        }
    }

    /// Start a child.
    ///
    /// Refuses up front when the prompt is empty or the token is already
    /// revoked: spawning a worker only to have it stop on its first check would
    /// produce a settlement for work that never had authority to begin with.
    ///
    /// # Errors
    /// [`ChildError::EmptyPrompt`] or [`ChildError::AlreadyRevoked`].
    pub async fn spawn(&self, spec: ChildSpec) -> Result<ChildHandle, ChildError> {
        if spec.prompt.trim().is_empty() {
            return Err(ChildError::EmptyPrompt);
        }
        if self.is_revoked(&spec.token).await {
            return Err(ChildError::AlreadyRevoked);
        }

        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        let cancelled = Arc::new(AtomicBool::new(false));

        let provider = Arc::clone(&self.provider);
        let deny_list = Arc::clone(&self.deny_list);
        let flag = Arc::clone(&cancelled);

        let worker = tokio::spawn(async move {
            let mut rounds: u32 = 0;
            let mut transcript = spec.prompt.clone();

            while rounds < spec.max_rounds {
                // ACTION BOUNDARY. All three checks happen before dispatch,
                // because each one makes the dispatch itself illegitimate:
                // a cancelled child must not start another round, a revoked
                // token no longer authorizes one, and an unaffordable round
                // would overrun the reservation it is supposed to be bounded by.
                if flag.load(Ordering::SeqCst) {
                    return ChildOutcome::Cancelled { rounds };
                }
                {
                    let deny = deny_list.lock().await;
                    if deny.is_revoked(&spec.token.revocation_ids()) {
                        return ChildOutcome::Revoked { rounds };
                    }
                }
                let projected = CostTuple {
                    cents: u64::from(spec.envelope.cents_max),
                    ..CostTuple::default()
                };
                if !spec.reservation.can_afford(&projected) {
                    return ChildOutcome::BudgetExhausted { rounds };
                }

                let request = CompletionRequest::new(
                    vec![ChatMessage::user(transcript.clone())],
                    spec.model.clone(),
                    spec.envelope.tokens_out_max,
                );

                // The child is cancellable *during* the call, not merely
                // between calls: a provider round can take tens of seconds, and
                // a cancel that only lands between rounds is not cancellation.
                let result = tokio::select! {
                    biased;
                    _ = &mut cancel_rx => {
                        flag.store(true, Ordering::SeqCst);
                        return ChildOutcome::Cancelled { rounds };
                    }
                    r = provider.complete(request) => r,
                };

                rounds += 1;

                match result {
                    Ok(response) => {
                        // Settle the REAL billed cost, not the projection: the
                        // reservation exists to bound spend, and bounding it
                        // against an estimate would let a cheap projection
                        // authorize an expensive round.
                        spec.reservation.record_spend(&response.cost);

                        if !response.content.trim().is_empty() {
                            return ChildOutcome::Completed {
                                text: response.content,
                                rounds,
                            };
                        }
                        transcript.push_str("\n(continue)");
                    }
                    Err(ProviderError::Unauthorized) => {
                        // Kept distinct from a transport fault: an auth failure
                        // is an authority problem an operator must act on, and
                        // flattening it into a generic failure hides that.
                        return ChildOutcome::Failed {
                            reason: "unauthorized".to_string(),
                            rounds,
                        };
                    }
                    Err(e) => {
                        return ChildOutcome::Failed {
                            reason: e.to_string(),
                            rounds,
                        };
                    }
                }
            }

            ChildOutcome::BudgetExhausted { rounds }
        });

        Ok(ChildHandle {
            cancel_tx: Some(cancel_tx),
            worker,
            cancelled,
        })
    }

    async fn is_revoked(&self, token: &CapToken) -> bool {
        self.deny_list
            .lock()
            .await
            .is_revoked(&token.revocation_ids())
    }

    /// Stop `child` after its authority has already been revoked.
    ///
    /// The caller revokes first — writing to the shared deny-list this
    /// supervisor reads — and only then calls this. That order matters: a
    /// worker that wakes during the stop re-checks the deny-list and finds the
    /// authority already gone. Stopping first and revoking after leaves a
    /// window in which the child still sees a live token and dispatches one
    /// more paid round.
    ///
    /// # Errors
    /// [`ChildError::WorkerLost`] if the worker panicked.
    pub async fn stop_revoked(&self, child: ChildHandle) -> Result<ChildOutcome, ChildError> {
        child.cancel().await
    }
}

/// How long to wait for a child to wind down before treating it as lost.
pub const DEFAULT_CANCEL_GRACE: Duration = Duration::from_secs(5);
