//! ardur-cap-token — Biscuit-backed capability tokens.
//!
//! Plan family: §11.14
//! (`plans/11.14-cost-ceilings-receipts-cap-tokens-blueprint.md`). Design
//! record: ADR-Phase3-547 (cap-tokens as Biscuits — offline attenuation by the
//! holder, decentralized verification against the issuer's public key, and a
//! logic language whose only attenuation primitive is *append a check*, so
//! widening a capability is unrepresentable by construction).
//!
//! # Phase 1 (this crate)
//!
//! - [`CapScope`] / [`HolderId`] — the issuance claims (audience, expiry,
//!   budget ceiling, tool allowlist) and the principal a token is bound to.
//! - [`CapTokenIssuer`] / [`BiscuitCapTokenIssuer`] — mint a root token whose
//!   authority block carries the claims plus the checks that bind any future
//!   request to them.
//! - [`CapTokenAttenuator`] / [`BiscuitCapTokenAttenuator`] — append a
//!   strictly-narrowing [`Caveat`] ([`AttenuationRule`]); the child's authority
//!   is the intersection of every block's checks.
//! - [`CapTokenVerifier`] / [`BiscuitCapTokenVerifier`] — re-bind the token to
//!   a root key, screen it against a [`DenyList`], and authorize it against a
//!   concrete request ([`RequiredCaveats`]), returning [`VerifiedClaims`].
//! - [`DenyList`] / [`HashSetDenyList`] / [`FileDenyList`] — revocation by
//!   Biscuit revocation id, either in-memory or persisted to a shared file.
//!
//! Biscuit's `KeyPair`/`PublicKey` support Ed25519 and P-256; they are
//! re-exported below so callers issue and verify against the same key types
//! and signature algorithm used by the token.
//!
//! # Security model (gh#363)
//!
//! - **Possession is authority.** A cap-token is a bearer credential: whoever
//!   presents a structurally valid, signature-verifying token holds its
//!   authority. There is no proof-of-possession binding (no DPoP, no mTLS) —
//!   if a token leaks in a log, a journal, or a captured request, the
//!   attacker can replay it until it expires or is revoked. Treat the base64
//!   string as a secret.
//! - **Short TTL as containment.** The server mints tokens with a 5-minute
//!   lifetime (`CAP_TTL_SECS = 5 * 60`, crates/server/src/state.rs), so a
//!   leaked token's replay window is bounded by that expiry plus the
//!   revocation check. Do not lengthen the TTL without a design note.
//! - **Revocation depends on the verifier's configured backend.** The server
//!   shares a [`FileDenyList`]-backed handle between its fused runtime and
//!   `delegate_task` at `<data_dir>/security/deny.list`. Acknowledged writes
//!   survive process restart and are consulted on later verification.
//!   [`HashSetDenyList`] and default library constructors remain process-local;
//!   this does not add a server HTTP revoke endpoint or immediate cancellation.
//!   Embedders must use the revocation writer API and propagate its I/O errors.
//!   A token or request is not single-use: no general nonce/replay cache ships.
//! - **Never log tokens.** This crate performs no logging; the base64 wire
//!   form must not be passed to a logger, a journal, or an error message
//!   that surfaces to a caller. A logged token is a leaked token.
//!
//! Phase 2 (see inline `// TODO §11.14 Phase 2:` markers) adds third-party
//! caveats and sealed tokens.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod attenuate;
mod denylist;
mod error;
mod issue;
pub mod pop;
mod types;
mod verify;

/// Re-exported Biscuit primitives. Cap-tokens are Biscuits under the hood;
/// callers that need to verify or inspect raw key material use these directly.
/// `KeyPair` and `PublicKey` support Ed25519 and P-256.
pub use biscuit_auth::{Biscuit, KeyPair, PublicKey};

pub use attenuate::{BiscuitCapTokenAttenuator, CapTokenAttenuator};
pub use denylist::{DenyList, FileDenyList, HashSetDenyList};
pub use error::CapTokenError;
pub use issue::{BiscuitCapTokenIssuer, CapTokenIssuer};
pub use pop::{
    Confirmation, KeyThumbprint, PopProof, PopRequirement, ReplayCache, RequestBinding, verify_pop,
};
pub use types::{
    AttenuationRule, CapScope, CapToken, Caveat, HolderId, RequiredCaveats, VerifiedClaims,
};
pub use verify::{BiscuitCapTokenVerifier, CapTokenVerifier};
