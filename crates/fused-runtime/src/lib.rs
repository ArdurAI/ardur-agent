//! ardur-fused-runtime — the Phase-2 fused [`ChatRuntime`] that wires every
//! Phase-1 substrate crate behind a single [`FusedRuntime::submit`] call.
//!
//! # What it fuses
//!
//! One [`submit`](FusedRuntime::submit) drives the turn through the stages
//! below, in order, short-circuiting on the first failure (stage 4.5 is the
//! injection-defense scan slotted between the pre-submit hooks and the provider):
//!
//! 1. **cap-token** ([`ardur_cap_token`]) — parse + verify the request's
//!    capability token against the root key, the audience, the tool, and the
//!    deny-list. A rejection is [`RuntimeError::CapDenied`] (or
//!    [`RuntimeError::CapTokenExpired`] for an expired token).
//! 2. **cedar-policy** ([`ardur_cedar_policy`]) — evaluate the turn against the
//!    policy bundle. The principal is *derived* from the stage-1 verified
//!    cap-token subject (never asserted by the caller) and the resource from the
//!    session; the cap claims ride as resource attributes. A `Deny`/
//!    `Indeterminate` is [`RuntimeError::PolicyDenied`].
//! 3. **cost-gate** ([`ardur_cost_gate`]) — admit the projected envelope against
//!    the holder's budget. A rejection is [`RuntimeError::CostCeilingExceeded`].
//! 4. **lifecycle-hooks** ([`ardur_lifecycle_hooks`]) — run the pre-submit hooks;
//!    a `Veto` aborts (and releases the reservation) as
//!    [`RuntimeError::VetoedByHook`], a `Replace` swaps the request.
//!    - **stage 4.5 — injection-defense** ([`ardur_injection_defense`], ARD-48):
//!      scan the (possibly hook-rewritten) outbound prompt through the
//!      [`FilterRegistry`](ardur_injection_defense::FilterRegistry). A `Block`
//!      aborts (releasing the reservation, minting no receipt) as
//!      [`RuntimeError::InjectionBlocked`]; an `AllowWithSanitization` swaps the
//!      provider body for the redacted rewrite (the raw prompt still rides to the
//!      journal). Wired via
//!      [`FusedRuntimeBuilder::with_injection_filters`](crate::FusedRuntimeBuilder::with_injection_filters);
//!      the default empty registry makes the stage a no-op.
//! 5. **provider-runtime** ([`ardur_provider_runtime`]) — dispatch the
//!    completion to the real [`Provider`](ardur_provider_runtime::Provider).
//! 6. **receipt** ([`ardur_receipt`]) — mint + sign the turn's receipt, chaining
//!    its `parent_hash` onto the prior receipt's JWS.
//! 7. **lifecycle-hooks** — run the post-receipt hooks (observational).
//! 8. **cost-gate** — finalize the reservation, refunding `reserved - actual`.
//! 9. **memory** ([`ardur_memory`]) — record the turn as a bi-temporal fact.
//! 10. **session-journals** ([`ardur_session_journals`]) — append the user +
//!     assistant messages to the durable, fsync-ing journal.
//!
//! Stages 6→10 happen *after* the provider has already produced a response — so
//! a failure there fires the relevant `on_error` hook but does not un-happen the
//! turn (the receipt is the source of truth). A failure at stages 1→5 returns
//! the error and never reaches the provider's billing.
//!
//! # Durable settlement ownership
//!
//! The builder requires a configured receipt log; its private sibling settlement
//! store is bound to the actual absolute path and full public signer JWKS. One
//! supervised economic execution owns admission, cumulative work, exact prepared
//! receipt bytes, actual debit/refund and capability retirement. Drop refunds
//! precommit cancellation synchronously without erasing known expense. Keep
//! [`FusedRuntime::settlement_supervisor`] externally and call its explicit async
//! `drain_pending` with the configured journal to project cancellation evidence.
//! Ambiguous journal/receipt acknowledgements fail closed, not blind rollback.
//! Prior-epoch uncertainty blocks dispatch; it never replays process-local money.
//! Bounds are four live/backlog slots, one executing turn, 64 retained snapshots,
//! five rounds, eight tools per round, 256 KiB records and 16 KiB receipt candidates.
//! There is no eviction, background worker or persistent budget database.
//!
//! # Governance ER mirror (#502 Seam B7, Phase 1 — opt-in)
//!
//! [`FusedRuntimeBuilder::with_governance`] wires an
//! [`ardur_governance::GovernanceEmitter`] into the commit decision: every round
//! whose native receipt durably commits is mirrored as a signed Ardur Execution
//! Receipt (see [`crate::governance`] and the shipped
//! [`ErMirrorEmitter`](crate::ErMirrorEmitter) file-backed implementation).
//! Abandoned / cancelled turns mint no ER, the default (no emitter configured)
//! is byte-identical to the pre-B7 runtime, and a mirror failure never un-commits
//! a round. Phase 1 mirrors round-level chat admission only; per-tool effect
//! classification needs #543's durable per-event evidence and stays out.
//!
//! # Streaming (§6.0c)
//!
//! [`FusedRuntime::stream`](FusedRuntime::stream) is the progressive sibling of
//! `submit`: it drives the **same** ten stages over the **same** helpers but
//! yields a [`FusedEvent`] feed as the turn unfolds — stage transitions, token
//! [`Content`](FusedEvent::Content) deltas as the provider emits them, the
//! tool-call lifecycle, the minted receipt's chain hash, and the terminal
//! [`Finish`](FusedEvent::Finish). A consumer (the CLI, a channel adapter) gets
//! the §2.1b streaming UX **without** the security/observability bypass a direct
//! [`Provider::stream`](ardur_provider_runtime::Provider::stream) call at the CLI
//! layer incurs. Because the whole pipeline runs inside the returned stream's
//! generator, dropping the stream cancels the in-flight provider round and mints
//! **no** receipt — see [`crate::streaming`] for the event model and the
//! cancellation contract.
//!
//! # Why a separate crate (Option B)
//!
//! The brief offered two shapes: **(A)** extend `ardur-lifecycle-hooks`'
//! `HookedRuntime`, or **(B)** a new crate. This is **B**, and here is why A does
//! not compose cleanly:
//!
//! - **Plan-family boundaries.** `HookedRuntime` lives in the §11.17 hooks crate,
//!   whose documented identity is "observe the provider call, the receipt, and
//!   the memory write". Teaching it cap-token verification, Cedar authorization,
//!   cost admission, and journal durability would pull four more plan-families'
//!   crates (§11.14 cap-token, §11.0 cedar, §11.14 cost-gate, §7.10 journals)
//!   into a crate that is supposed to own only §11.17 — conflating one
//!   plan-family with the cross-cutting Phase-2 fusion.
//! - **The receipt chain.** `HookedRuntime::submit` mints a genesis-only receipt
//!   (`parent_hash: None` every turn). The fused runtime must chain receipts
//!   across turns *and across process restarts*, which `HookedRuntime` cannot do
//!   without rework.
//!
//! So the fusion sits in its own crate at the top of the dependency graph,
//! **reusing** `ardur-lifecycle-hooks`'
//! [`HookRegistry`](ardur_lifecycle_hooks::HookRegistry) and its context types
//! for the hook stages rather than re-deriving the composition rules.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod builder;
mod governance;
mod receipt_cache;
mod receipts;
mod reconcile;
mod runtime;
pub mod settlement;
mod shared;
pub mod streaming;

pub use builder::FusedRuntimeBuilder;
pub use governance::ErMirrorEmitter;
pub use receipt_cache::{LoadedReceiptChain, ReceiptCacheStats, VerifiedReceiptCache};
pub use receipts::{
    ControlReceiptWriter, PersistedReceipt, ReceiptChainError, load_persisted_chain,
    mint_control_receipt, verify_persisted_chain, verify_persisted_chain_with_jwks,
};
pub use reconcile::{
    ReconciliationAction, ReconciliationError, ReconciliationReport, ReconciliationStrategy,
};
pub use runtime::{
    BackgroundTaskOutcome, CancelProbe, CheckpointInfo, CheckpointOutcome, CompactOutcome,
    FusedRuntime, PerRequestProvisioning, RollbackOutcome, TurnCommitHandshake,
};
pub use shared::{SharedBudget, SharedDenyList};
pub use streaming::{FusedEvent, StageKind};
