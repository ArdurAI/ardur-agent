//! ardur-multi-agent — the §5.0 sub-agent runtime.
//!
//! Plan family: §5.0 (`plans/5.0-multi-agent-runtime-blueprint.md`). Receipt
//! shape: §5.7 subagent receipt vocab (ADR-Phase3-234..237) — a sub-agent's
//! lifecycle is auditable from its parent's chain because a
//! [`TerminationReceipt`] links back via `parent_receipt_id` and reports cost in
//! the §11.14 receipt [`CostTuple`] vocab.
//!
//! # Phase 1 (this crate)
//!
//! A sub-agent wraps a child §1.0 [`ChatRuntime`] with three isolation
//! mechanisms, each leaning on an existing crate rather than re-deriving it:
//!
//! - **Attenuated authority.** [`MultiAgentRuntime::spawn`] narrows the parent's
//!   §11.14 cap-token by each [`AttenuationRule`] before the sub-agent runs.
//!   Because Biscuit checks only ever intersect, the child is strictly narrower
//!   than the parent on every axis — a sub-agent can never widen what it was
//!   granted. The §5.1 [`CapVerifyingRuntime`] gives that narrowing teeth at
//!   request time: it re-binds the presented token to the issuer root and
//!   authorizes it for [`CHAT_SUBMIT_TOOL`] on every turn, so a sub-agent whose
//!   tool, audience, or expiry was attenuated away is actually denied at
//!   [`submit`](ChatRuntime::submit) rather than merely looking narrower to an
//!   auditor. Build one with [`InMemoryMultiAgentRuntime::verifying`].
//! - **Isolated budget.** Each [`SubAgent`] meters its lifetime spend against a
//!   §11.14 [`CostEnvelope`]; [`MultiAgentRuntime::ask`] reserves a turn's
//!   declared cost *before* invoking the child, so an over-budget turn returns
//!   [`MultiAgentError::BudgetExhausted`] without ever reaching the model.
//! - **Isolated session + receipt chain.** Each sub-agent runs in its own
//!   [`SessionId`], and termination emits a [`TerminationReceipt`] linking the
//!   parent's audit chain.
//!
//! [`InMemoryMultiAgentRuntime`] is the Phase-1 default, holding the registry in
//! a `parking_lot`-guarded map.
//!
//! # Reconciliation: generic child runtime, `?Send` trait
//!
//! The §5.0 contract names the child runtime `Box<dyn ChatRuntime>`. §1.0's
//! [`ChatRuntime::submit`] returns a return-position `impl Future`, which makes
//! the trait neither dyn-compatible nor Send-bounded. So [`SubAgent`] is generic
//! over the concrete child runtime (`Arc<R>`) and [`MultiAgentRuntime`] is
//! `#[async_trait(?Send)]`. The trait stays object-safe (`dyn
//! MultiAgentRuntime` is usable); only thread-spawning the futures is given up.
//!
//! Phase 2 (see inline `// TODO §5.0 Phase 2:` markers) adds the cross-agent
//! message bus, persistent sub-agent state, hierarchy depth limits, and dynamic
//! budget rebalancing.
//!
//! [`ChatRuntime`]: ardur_runtime::ChatRuntime
//! [`ChatRuntime::submit`]: ardur_runtime::ChatRuntime::submit
//! [`SessionId`]: ardur_runtime::SessionId
//! [`CostEnvelope`]: ardur_cost_gate::CostEnvelope
//! [`AttenuationRule`]: ardur_cap_token::AttenuationRule
//! [`CostTuple`]: ardur_receipt::CostTuple
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod agent;
mod child;
mod error;
mod runtime;
mod types;

pub use agent::SubAgent;
pub use child::{CHAT_SUBMIT_TOOL, CapVerifyingRuntime};
pub use error::MultiAgentError;
pub use runtime::{InMemoryMultiAgentRuntime, MultiAgentRuntime};
pub use types::{
    AgentId, PrincipalRef, SubAgentHandle, SubAgentRequest, SubAgentResponse, SubAgentSpec,
    TerminationReason, TerminationReceipt,
};

// --- Re-exported sibling-crate surface, so a caller wires sub-agents from one
//     import root. The sub-agent layer never redefines these — it speaks the
//     same cap-token, cost, receipt, and runtime types its siblings own. ---

/// §11.14 cap-token surface: issuance, attenuation, and verification of a
/// sub-agent's narrowed authority.
pub use ardur_cap_token::{
    AttenuationRule, BiscuitCapTokenAttenuator, BiscuitCapTokenIssuer, BiscuitCapTokenVerifier,
    CapScope, CapToken, CapTokenAttenuator, CapTokenError, CapTokenIssuer, CapTokenVerifier,
    Caveat, HashSetDenyList, HolderId, KeyPair, PublicKey, RequiredCaveats, VerifiedClaims,
};
/// §11.14 cost-gate surface: the per-sub-agent lifetime budget ceiling.
pub use ardur_cost_gate::CostEnvelope;
/// §11.14 receipt surface: the cost/timestamp vocab termination receipts report
/// in.
pub use ardur_receipt::{CostTuple, UnixTsMillis};
/// §1.0 runtime surface: the child chat runtime and the shared turn types.
pub use ardur_runtime::{
    CapTokenRef, ChatMessage, ChatRuntime, InMemoryRuntime, ReceiptId, Role, RuntimeError,
    SessionId,
};
