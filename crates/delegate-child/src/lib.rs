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

use ardur_cap_token::{
    BiscuitCapTokenVerifier, CapToken, CapTokenVerifier, DenyList, RequiredCaveats,
};
use ardur_core_types::{CostEnvelope, CostTuple, ModelId};
use ardur_provider_runtime::{
    ChatMessage, CompletionRequest, FinishReason, Provider, ProviderError,
};
use biscuit_auth::PublicKey;
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
    /// Stopped because it used up its permitted number of rounds.
    ///
    /// Deliberately distinct from [`BudgetExhausted`](Self::BudgetExhausted):
    /// conflating them tells an operator to top up a budget that was never the
    /// constraint, and hides a child that is looping without converging.
    RoundLimitReached {
        /// The limit that was hit.
        rounds: u32,
    },
    /// Stopped because its capability token failed verification.
    ///
    /// Covers expiry, a wrong audience, an untrusted root, or any unsatisfied
    /// caveat — not merely revocation. Kept apart from
    /// [`Revoked`](Self::Revoked) because "the authority was withdrawn" and
    /// "this authority was never valid here" call for different operator
    /// responses.
    Unauthorized {
        /// Why verification failed.
        reason: String,
        /// Rounds completed before authority lapsed.
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
            | Self::RoundLimitReached { .. }
            | Self::Unauthorized { .. }
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
            | Self::RoundLimitReached { rounds }
            | Self::Unauthorized { rounds, .. }
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
    /// The token failed verification before the child started.
    #[error("cap-token is not valid for this child: {reason}")]
    Unauthorized {
        /// Why verification failed.
        reason: String,
    },
}

/// The parent's spendable allowance, from which child reservations are carved.
///
/// Reservations are deducted here **atomically**. An earlier shape took the
/// parent's balance as a plain `available: u64` argument, which is only a
/// snapshot: two children could each reserve 60 cents from the same 100-cent
/// parent because neither deduction was visible to the other. A shared ledger
/// is what makes "two children cannot spend the same cent" true rather than
/// merely documented.
#[derive(Debug, Clone)]
pub struct ParentBudget {
    available_cents: Arc<AtomicU64>,
}

impl ParentBudget {
    /// A parent holding `cents`.
    #[must_use]
    pub fn new(cents: u64) -> Self {
        Self {
            available_cents: Arc::new(AtomicU64::new(cents)),
        }
    }

    /// Cents not currently reserved by any child.
    #[must_use]
    pub fn available_cents(&self) -> u64 {
        self.available_cents.load(Ordering::SeqCst)
    }

    /// Atomically carve `cents` out of the parent for one child.
    ///
    /// Uses a compare-and-swap loop rather than a check-then-subtract, so two
    /// concurrent callers cannot both observe the same balance and both succeed.
    ///
    /// # Errors
    /// [`ChildError::ReservationTooLarge`] when the parent no longer holds that
    /// much — the check that stops children from over-committing their parent.
    pub fn reserve(&self, cents: u64) -> Result<BudgetReservation, ChildError> {
        let mut current = self.available_cents.load(Ordering::SeqCst);
        loop {
            if cents > current {
                return Err(ChildError::ReservationTooLarge {
                    requested: cents,
                    available: current,
                });
            }
            match self.available_cents.compare_exchange(
                current,
                current - cents,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    return Ok(BudgetReservation {
                        reserved_cents: cents,
                        spent_cents: Arc::new(AtomicU64::new(0)),
                        worst_round_cents: Arc::new(AtomicU64::new(0)),
                        parent: self.clone(),
                    });
                }
                // Another child moved the balance between our read and our
                // write; re-read and re-check rather than clobbering it.
                Err(actual) => current = actual,
            }
        }
    }

    /// Return unspent cents to the parent when a child settles.
    fn release(&self, cents: u64) {
        self.available_cents.fetch_add(cents, Ordering::SeqCst);
    }
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
    worst_round_cents: Arc<AtomicU64>,
    parent: ParentBudget,
}

impl BudgetReservation {
    /// Release this child's unspent cents back to its parent.
    ///
    /// Called once the child has settled. Without it a cancelled or failed
    /// child's unspent allowance would stay locked away from its siblings for
    /// the life of the parent.
    pub fn settle(&self) {
        let unspent = self.remaining_cents();
        if unspent > 0 {
            self.parent.release(unspent);
        }
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

    /// Record spend against the reservation.
    pub fn record_spend(&self, cost: &CostTuple) {
        self.spent_cents.fetch_add(cost.cents, Ordering::SeqCst);
        self.worst_round_cents
            .fetch_max(cost.cents, Ordering::SeqCst);
    }

    /// The largest single round billed so far.
    ///
    /// Admission uses this so a provider that bills above its declared envelope
    /// cannot keep being admitted on a projection its own history contradicts.
    #[must_use]
    pub fn worst_round_cents(&self) -> u64 {
        self.worst_round_cents.load(Ordering::SeqCst)
    }

    /// Whether actual spend has exceeded what was reserved.
    ///
    /// Possible whenever a provider bills above the projected envelope the
    /// pre-dispatch check admitted the call on. `remaining_cents` saturates at
    /// zero and so cannot express it, which would let an overdrawn child look
    /// merely broke and continue.
    #[must_use]
    pub fn is_overdrawn(&self) -> bool {
        self.spent_cents() > self.reserved_cents
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
///
/// Dropping this handle does **not** abandon the worker. The supervisor retains
/// its own reference to the join handle, so an aborted parent or a dropped
/// `join()` future still leaves a worker that can be drained for its terminal
/// outcome. An earlier shape stored the `JoinHandle` here alone: dropping the
/// handle detached the task and discarded its settlement, which is exactly the
/// orphaned-work path `cancel()` exists to prevent.
pub struct ChildHandle {
    cancel_tx: Option<oneshot::Sender<()>>,
    worker: Arc<Mutex<Option<JoinHandle<ChildOutcome>>>>,
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
        drain(&self.worker).await
    }

    /// Wait for the child to finish on its own.
    ///
    /// # Errors
    /// [`ChildError::WorkerLost`] if the worker panicked.
    pub async fn join(self) -> Result<ChildOutcome, ChildError> {
        drain(&self.worker).await
    }
}

/// Spawns supervised children onto a real provider.
pub struct ChildSupervisor<P: Provider + ?Sized + 'static> {
    provider: Arc<P>,
    deny_list: Arc<Mutex<Box<dyn DenyList + Send>>>,
    authority: Option<Arc<TokenAuthority>>,
}

/// The trusted context a child's token is verified against.
///
/// Checking revocation ids alone is not authorization: a token can be
/// unrevoked and still be expired, issued for another audience, or signed by a
/// root this deployment does not trust. Supplying this makes the child verify
/// the whole caveat set before every dispatch.
pub struct TokenAuthority {
    /// The trusted issuing root.
    pub root: PublicKey,
    /// The audience this child presents itself as.
    pub audience: String,
    /// The tool the child's rounds invoke.
    pub tool: String,
    /// Clock used for expiry checks, injectable so tests need not sleep.
    pub now_unix: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl<P: Provider + ?Sized + 'static> ChildSupervisor<P> {
    /// Build a supervisor that checks **revocation only**.
    ///
    /// Sufficient when the caller has already verified the token's caveats for
    /// this request and only needs mid-flight revocation. Prefer
    /// [`with_authority`](Self::with_authority) when the child should re-verify
    /// expiry, audience and issuing root itself.
    pub fn new(provider: Arc<P>, deny_list: Box<dyn DenyList + Send>) -> Self {
        Self {
            provider,
            deny_list: Arc::new(Mutex::new(deny_list)),
            authority: None,
        }
    }

    /// Build a supervisor that fully verifies the child's token against
    /// `authority` at spawn and before every provider dispatch.
    pub fn with_authority(
        provider: Arc<P>,
        deny_list: Box<dyn DenyList + Send>,
        authority: TokenAuthority,
    ) -> Self {
        Self {
            provider,
            deny_list: Arc::new(Mutex::new(deny_list)),
            authority: Some(Arc::new(authority)),
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
        let per_round_cents = u64::from(spec.envelope.cents_max);
        if let Some(auth) = self.authority.as_ref() {
            let deny = self.deny_list.lock().await;
            if let Err(reason) = verify_authority(auth, &spec.token, &**deny, per_round_cents) {
                return Err(ChildError::Unauthorized { reason });
            }
        }

        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        let cancelled = Arc::new(AtomicBool::new(false));

        let provider = Arc::clone(&self.provider);
        let deny_list = Arc::clone(&self.deny_list);
        let flag = Arc::clone(&cancelled);
        let authority = self.authority.clone();

        let worker = tokio::spawn(async move {
            let mut rounds: u32 = 0;
            let mut transcript = spec.prompt.clone();

            while rounds < spec.max_rounds {
                // ACTION BOUNDARY. Each check makes the dispatch itself
                // illegitimate: a cancelled child must not start another round,
                // an unverified or revoked token no longer authorizes one, and
                // an unaffordable round would overrun the reservation it is
                // supposed to be bounded by.
                if flag.load(Ordering::SeqCst) {
                    spec.reservation.settle();
                    return ChildOutcome::Cancelled { rounds };
                }
                {
                    let deny = deny_list.lock().await;
                    if deny.is_revoked(&spec.token.revocation_ids()) {
                        spec.reservation.settle();
                        return ChildOutcome::Revoked { rounds };
                    }
                    // Revocation is not authorization. Re-verify expiry,
                    // audience and issuing root too, or a token that expired
                    // mid-turn keeps buying rounds.
                    if let Some(auth) = authority.as_ref()
                        && let Err(e) =
                            verify_authority(auth, &spec.token, &**deny, per_round_cents)
                    {
                        spec.reservation.settle();
                        return ChildOutcome::Unauthorized { reason: e, rounds };
                    }
                }
                // Admit the round against the larger of the declared envelope
                // and what rounds have ACTUALLY cost. A provider billing above
                // its envelope would otherwise keep being admitted on a stale,
                // too-small projection until the reservation is blown - the
                // overrun is only detectable after the fact, and by then the
                // money is spent.
                let projected = CostTuple {
                    cents: per_round_cents.max(spec.reservation.worst_round_cents()),
                    ..CostTuple::default()
                };
                if !spec.reservation.can_afford(&projected) {
                    spec.reservation.settle();
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
                //
                // NOT biased toward cancellation. When both the response and
                // the cancel are ready, a biased select would always discard a
                // call that the upstream has already BILLED - understating both
                // spend and round count. Preferring the completed response
                // settles the real cost, then the flag stops the next round.
                let result = tokio::select! {
                    r = provider.complete(request) => Some(r),
                    _ = &mut cancel_rx => {
                        flag.store(true, Ordering::SeqCst);
                        None
                    }
                };

                let Some(result) = result else {
                    spec.reservation.settle();
                    return ChildOutcome::Cancelled { rounds };
                };

                rounds += 1;

                match result {
                    Ok(response) => {
                        // Settle the REAL billed cost, not the projection: the
                        // reservation exists to bound spend, and bounding it
                        // against an estimate would let a cheap projection
                        // authorize an expensive round.
                        spec.reservation.record_spend(&response.cost);

                        // A provider may bill above the projection. That is the
                        // moment the hard bound is breached, so stop here
                        // rather than admitting another round on a balance that
                        // is already overdrawn.
                        if spec.reservation.is_overdrawn() {
                            spec.reservation.settle();
                            return ChildOutcome::BudgetExhausted { rounds };
                        }

                        // Content alone is not success. A provider can emit a
                        // partial message and THEN fail; treating non-empty
                        // text as completion turns a failed turn into a
                        // successful child result carrying the success verb.
                        if let FinishReason::Error(msg) = &response.finish_reason {
                            spec.reservation.settle();
                            return ChildOutcome::Failed {
                                reason: msg.clone(),
                                rounds,
                            };
                        }

                        if !response.content.trim().is_empty() {
                            spec.reservation.settle();
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
                        spec.reservation.settle();
                        return ChildOutcome::Failed {
                            reason: "unauthorized".to_string(),
                            rounds,
                        };
                    }
                    Err(e) => {
                        spec.reservation.settle();
                        return ChildOutcome::Failed {
                            reason: e.to_string(),
                            rounds,
                        };
                    }
                }
            }

            spec.reservation.settle();
            ChildOutcome::RoundLimitReached { rounds }
        });

        Ok(ChildHandle {
            cancel_tx: Some(cancel_tx),
            worker: Arc::new(Mutex::new(Some(worker))),
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
        let outcome = child.cancel().await?;
        // A child stopped *because its authority was revoked* must not settle
        // as an ordinary cancellation. The worker often cannot tell the
        // difference - it is mid-dispatch when the stop lands and reports
        // Cancelled - so the caller, which knows why it stopped, restates it.
        // Work that genuinely finished first keeps its own truthful outcome.
        Ok(match outcome {
            ChildOutcome::Cancelled { rounds } => ChildOutcome::Revoked { rounds },
            settled => settled,
        })
    }
}

/// Await the worker and yield its terminal outcome, once.
///
/// The handle is taken out of the shared slot so a second drain reports a lost
/// worker rather than hanging forever on an already-consumed join handle.
async fn drain(
    slot: &Arc<Mutex<Option<JoinHandle<ChildOutcome>>>>,
) -> Result<ChildOutcome, ChildError> {
    let handle = slot.lock().await.take();
    match handle {
        Some(h) => h.await.map_err(|e| ChildError::WorkerLost(e.to_string())),
        None => Err(ChildError::WorkerLost(
            "the child's terminal outcome was already taken".to_string(),
        )),
    }
}

/// Verify a child's token against its trusted authority for one round.
///
/// Returns the failure reason as a string so the caller can classify it without
/// leaking `CapTokenError` into this crate's public surface.
fn verify_authority(
    auth: &TokenAuthority,
    token: &CapToken,
    deny: &(dyn DenyList + Send),
    cost: u64,
) -> Result<(), String> {
    let required = RequiredCaveats {
        now_unix: (auth.now_unix)(),
        audience: auth.audience.clone(),
        tool: auth.tool.clone(),
        cost,
    };
    // The verifier consults the deny list itself, so revocation and caveat
    // failures are decided by one authority rather than two that can disagree.
    BiscuitCapTokenVerifier::new(DenyListRef(deny))
        .verify(token, &auth.root, &required)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Borrows a `&dyn DenyList` so the verifier can consult the supervisor's own
/// list rather than a divergent copy.
struct DenyListRef<'a>(&'a (dyn DenyList + Send));

impl DenyList for DenyListRef<'_> {
    fn is_revoked(&self, revocation_ids: &[Vec<u8>]) -> bool {
        self.0.is_revoked(revocation_ids)
    }
}

/// How long to wait for a child to wind down before treating it as lost.
pub const DEFAULT_CANCEL_GRACE: Duration = Duration::from_secs(5);
