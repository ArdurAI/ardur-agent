//! ardur-lifecycle-hooks — the §11.17 lifecycle-hook substrate (Phase 1).
//!
//! Plan family: §11.17
//! (`plans/11.17-lifecycle-hooks-deterministic-policy-blueprint.md`). Design
//! records: ADR-Phase3-576 (event taxonomy), 577 (wire protocol), 578
//! (composition rules), 579 (cap-token / Cedar gate).
//!
//! # Phase 1 (this crate)
//!
//! An **observer-with-veto** substrate wired into the turn lifecycle. Phase 1
//! is the in-process trait + registry + runtime wiring; receipt replay,
//! shell-script hosting, and the Cedar registration gate are later phases.
//!
//! - [`LifecycleHook`] — the object-safe async hook trait. Four lifecycle
//!   moments: [`on_pre_submit`](LifecycleHook::on_pre_submit) (the veto/rewrite
//!   point), [`on_post_receipt`](LifecycleHook::on_post_receipt),
//!   [`on_error`](LifecycleHook::on_error),
//!   [`on_revoke`](LifecycleHook::on_revoke).
//! - [`HookDecision`] — `Continue` / `Veto` / `Replace`, what a pre-submit hook
//!   returns.
//! - [`HookRegistry`] — priority-ordered hooks plus the composition rules
//!   ([`run_pre_submit`](HookRegistry::run_pre_submit) is first-veto-wins +
//!   replacements-chain; the observational runs collect errors).
//! - [`HookedRuntime`] — a [`ChatRuntime`](ardur_runtime::ChatRuntime) that
//!   threads the registry through a [`Provider`](ardur_provider_runtime::Provider)
//!   call, the minted signed receipt, and an optional
//!   [`MemoryRuntime`](ardur_memory::MemoryRuntime) write.
//! - [`RecordingHook`] — a built-in observer that logs every callback, for
//!   tests.
//!
//! Why a separate crate rather than living in `ardur-runtime`: the substrate
//! must observe the provider call, the receipt, and the memory write, and
//! `ardur-provider-runtime` / `ardur-receipt` / `ardur-memory` all already
//! depend on `ardur-runtime`. Wiring them together therefore sits *above* them
//! — keeping the dependency graph acyclic. The only `ardur-runtime` change is
//! the [`RuntimeError::VetoedByHook`](ardur_runtime::RuntimeError::VetoedByHook)
//! variant a veto surfaces.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod hook;
mod recording;
mod registry;
mod runtime;

pub use hook::{
    ErrorCtx, HookDecision, HookError, HookId, LifecycleHook, LifecyclePhase, PostReceiptCtx,
    PreSubmitCtx, RevokeCtx,
};
pub use recording::{EventLog, HookEvent, RecordingHook};
pub use registry::{HookRegistry, PreSubmitOutcome};
pub use runtime::HookedRuntime;
