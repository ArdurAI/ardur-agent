//! ardur-fused-runtime — the Phase-2 fused [`ChatRuntime`] that wires every
//! Phase-1 substrate crate behind a single [`FusedRuntime::submit`] call.
//!
//! # What it fuses
//!
//! One [`submit`](FusedRuntime::submit) drives the turn through ten stages, in
//! order, short-circuiting on the first failure:
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
mod receipts;
mod reconcile;
mod runtime;
mod shared;

pub use builder::FusedRuntimeBuilder;
pub use receipts::{
    PersistedReceipt, ReceiptChainError, load_persisted_chain, verify_persisted_chain,
};
pub use reconcile::{
    ReconciliationAction, ReconciliationError, ReconciliationReport, ReconciliationStrategy,
};
pub use runtime::{FusedRuntime, PerRequestProvisioning};
pub use shared::{SharedBudget, SharedDenyList};
