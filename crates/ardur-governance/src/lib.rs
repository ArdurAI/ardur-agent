//! ardur-governance — run ardur-agent as an Ardur-governed workload.
//!
//! This crate is the **seam** between ardur-agent and the Ardur governance
//! layer (MCEP — Mission-Controlled Execution Protocol). It projects the
//! agent's own authorization facts — a verified `ardur_cap_token` grant plus a
//! normalized tool invocation — into an **Ardur-verifiable Execution Receipt
//! (ER v0.1)**, and derives the kernel-enforcement policy that mirrors the same
//! authority.
//!
//! # What it does
//!
//! - [`project::project_execution_receipt`] builds an [`er::ExecutionReceipt`]
//!   claim set (field-for-field with `execution-receipt-v0.1.schema.json`) from
//!   verified cap-token claims + the invocation + the tri-state
//!   [`project::AuthOutcome`]. `grant_id` is the cap-token `token_id`; the
//!   verdict/denial mapping follows the verifier-contract §9 fail-closed table.
//! - [`sign::ErSigner`] / [`sign::ErVerifier`] sign and verify the ER as an
//!   ES256 JWS (`typ=application/ardur.er+jwt`), and [`sign::verify_er_chain`]
//!   checks a hash-chained **mirror log** — the ER equivalent of the native
//!   `ardur-receipt` chain, projected alongside it rather than replacing it.
//! - [`grant::GrantDescriptor`] carries the cap-token as the delegation grant
//!   plus an optional [`grant::MissionRef`] (the one DG-profile claim), ready
//!   to present to the Ardur proxy's `token_type=biscuit` session-start path.
//! - [`enforce::EnforcementProfile`] derives an Ardur `DaemonApplyPolicyRequest`
//!   from the verified capability set, and [`enforce::EnforcementAttach`] is the
//!   handoff seam to `ardur-kernelcaptured`.
//!
//! # What it does not do (cross-repo dependencies)
//!
//! - It does **not** mint JWT-AAT Delegation Grants (the agent has no Ed25519
//!   JWT-AAT issuer; the grant is a Biscuit cap-token). Full DG-chain
//!   verification per AAT §7 needs an Ardur-published Rust AAT surface or an
//!   agent-side JWT-AAT issuer.
//! - It does **not** perform kernel enforcement itself: true BPF-LSM deny is
//!   Linux + managed-cgroup only, and the daemon IPC contract is not yet
//!   published for a non-Go workload.
//!
//! These, and the identity/mission conventions, are tracked as CR-1..CR-5 in
//! the design write-up (`DESIGN.md`, alongside this crate).
//!
//! # Non-invasive by construction
//!
//! Nothing here mutates the native receipt path. The agent's `crates/receipt`
//! chain (single-writer under the fused runtime's `commit_lock`) is left
//! untouched; the ER is a projection of the same facts into the Ardur wire
//! format. A governed runtime signs both with one P-256 key and publishes one
//! JWKS.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod enforce;
mod er;
mod error;
mod grant;
mod hash;
mod jcs;
mod project;
mod sign;

pub use enforce::{
    EnforceAction, EnforceMode, EnforceOp, EnforcementAttach, EnforcementProfile, OpPolicy,
    RecordingAttach,
};
pub use er::{
    ActionClass, Canonicalization, DigestAlg, DigestObject, DigestScope, EvidenceLevel,
    ExecutionReceipt, PolicyDecision, PublicDenialReason, SideEffectClass, Verdict,
};
pub use error::GovernanceError;
pub use grant::{GrantDescriptor, MissionRef};
pub use project::{
    AuthOutcome, StepContext, ToolInvocation, check_verdict_invariant, project_execution_receipt,
};
pub use sign::{
    ER_TYP, ErSigner, ErSigningKey, ErVerifier, SignedExecutionReceipt, verify_er_chain,
};

/// The RFC 8785 (JCS) canonicalizer used for ER digests, exposed for callers
/// that need to canonicalize an invocation the same way the projector does.
pub mod canonicalization {
    pub use crate::jcs::to_canonical_bytes;
}
