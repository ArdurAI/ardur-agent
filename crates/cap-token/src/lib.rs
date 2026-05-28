//! ardur-cap-token — Biscuit cap-token issuance + attenuation.
//!
//! Plan family: §11.14 (`plans/11.14-cost-ceilings-receipts-cap-tokens-blueprint.md`).
//!
//! PHASE 0: contracts only. No implementation bodies — every trait method is
//! `unimplemented!()`. The public trait surface here is FROZEN against the
//! §11.14 Contract Surface; widening it is a §0.0 amendment, not a per-crate
//! decision. Bodies land in §11.14 Phase 1.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::Result;

/// Re-exported Biscuit primitives. Cap-tokens are Biscuits under the hood;
/// callers that need to verify or inspect raw key material use these directly.
pub use biscuit_auth::{Biscuit, KeyPair, PublicKey};

/// The principal a cap-token is issued to (a runtime profile, agent, or
/// session). Opaque string identifier at Phase 0.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HolderId(pub String);

/// The capability scope a cap-token grants — the cost ceilings, allowed
/// verbs, and resource bounds. Fields are derived from §11.14 in Phase 1.
#[derive(Clone, Debug, Default)]
pub struct CapScope {
    // TODO(§11.14 Phase 1): cost-ceiling template, allowed-verb set, expiry.
}

/// A single strictly-narrowing caveat applied during attenuation. Widening a
/// capability is unrepresentable by construction. Fields land in Phase 1.
#[derive(Clone, Debug, Default)]
pub struct Caveat {
    // TODO(§11.14 Phase 1): the narrowing fact (verb / cost / time bound).
}

/// An opaque, signed cap-token. Wraps a Biscuit; the raw token bytes are
/// never exposed except through the Biscuit re-export above.
#[derive(Clone, Debug)]
pub struct CapToken(pub Biscuit);

/// Issue a root cap-token for a holder from a capability scope (`cap.issue`).
/// Emits `cap.issued.v1` once §11.14 lands its body.
pub trait CapTokenIssuer {
    /// Issue a fresh root token granting `scope` to `holder`.
    fn issue(&self, holder: HolderId, scope: CapScope) -> Result<CapToken> {
        let _ = (holder, scope);
        unimplemented!("Phase 0 contract — body lands in §11.14 Phase 1")
    }
}

/// Narrow an existing cap-token by applying a caveat (`cap.attenuate`),
/// producing a child token with strictly narrower authority. Emits
/// `cap.attenuated.v1` once §11.14 lands its body.
pub trait CapTokenAttenuator {
    /// Produce a strictly-narrower child token from `token` under `caveat`.
    fn attenuate(&self, token: &CapToken, caveat: Caveat) -> Result<CapToken> {
        let _ = (token, caveat);
        unimplemented!("Phase 0 contract — body lands in §11.14 Phase 1")
    }
}
