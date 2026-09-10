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
//! Biscuit's `KeyPair`/`PublicKey` are Ed25519; they are re-exported below so
//! callers issue and verify against the same key types the token is signed
//! with.
//!
//! Phase 2 (see inline `// TODO §11.14 Phase 2:` markers) adds third-party
//! caveats and sealed tokens.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod attenuate;
mod denylist;
mod error;
mod issue;
mod types;
mod verify;

/// Re-exported Biscuit primitives. Cap-tokens are Biscuits under the hood;
/// callers that need to verify or inspect raw key material use these directly.
/// `KeyPair` and `PublicKey` are Ed25519.
pub use biscuit_auth::{Biscuit, KeyPair, PublicKey};

pub use attenuate::{BiscuitCapTokenAttenuator, CapTokenAttenuator};
pub use denylist::{DenyList, FileDenyList, HashSetDenyList};
pub use error::CapTokenError;
pub use issue::{BiscuitCapTokenIssuer, CapTokenIssuer};
pub use types::{
    AttenuationRule, CapScope, CapToken, Caveat, HolderId, RequiredCaveats, VerifiedClaims,
};
pub use verify::{BiscuitCapTokenVerifier, CapTokenVerifier};
