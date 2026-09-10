//! Delegation-Grant (DG) carry.
//!
//! The agent's grant credential is a `ardur_cap_token` Biscuit (Ed25519). The
//! MCEP DG *profile* (delegation-grant-profile-v0.1) is normatively a JWT-AAT
//! and adds exactly one claim, `mission_ref`, binding the grant chain to a
//! Mission Declaration. This crate does **not** mint JWT-AATs (see the
//! cross-repo dependency CR-1 in the session journal); instead it carries the
//! verified cap-token as the grant and pairs it with an optional `mission_ref`,
//! producing a [`GrantDescriptor`] a governed runtime can (a) stamp into every
//! Execution Receipt's `grant_id`, and (b) present to the Ardur proxy's
//! `token_type=biscuit` `/session/start` path.

use ardur_cap_token::VerifiedClaims;
use serde::{Deserialize, Serialize};

/// The DG-profile `mission_ref` claim (spec §3.2): a bare URI/JWK-thumbprint
/// string, or an object with a `uri` and an optional `mission_digest`
/// (`sha-256:` + 64 lowercase hex over the JCS-canonical MD payload).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MissionRef {
    /// String form — a URI or RFC 9278 JWK Thumbprint URI.
    Uri(String),
    /// Object form — `uri` plus optional digest binding.
    Object {
        /// The governing Mission Declaration identifier.
        uri: String,
        /// `sha-256:` + 64 lowercase hex over the RFC 8785 canonical MD payload.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        mission_digest: Option<String>,
    },
}

impl MissionRef {
    /// The referenced MD identifier (the `uri`, in either form).
    pub fn uri(&self) -> &str {
        match self {
            MissionRef::Uri(u) => u,
            MissionRef::Object { uri, .. } => uri,
        }
    }
}

/// A verified grant, projected for Execution-Receipt stamping and for
/// presentation to an Ardur verifier. Binds the cap-token grant id to the
/// governing mission and records the effective (post-attenuation) authority the
/// Biscuit verifier enforced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantDescriptor {
    /// The grant id stamped into every ER `grant_id` — the cap-token
    /// `VerifiedClaims.token_id` (in v0.1 the AAT `jti` analog).
    pub grant_id: String,
    /// The principal the grant is bound to (SPIFFE-style URI).
    pub subject: String,
    /// The governing mission, if bound.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mission_ref: Option<MissionRef>,
    /// Effective tool allowlist after attenuation intersection.
    pub effective_tools: Vec<String>,
    /// Effective spend ceiling remaining.
    pub budget_remaining: u64,
    /// Effective expiry (Unix seconds).
    pub expires_unix: u64,
}

impl GrantDescriptor {
    /// Build a descriptor from verified cap-token claims and an optional
    /// mission binding.
    pub fn from_claims(claims: &VerifiedClaims, mission_ref: Option<MissionRef>) -> Self {
        Self {
            grant_id: claims.token_id.to_string(),
            subject: claims.subject.0.clone(),
            mission_ref,
            effective_tools: claims.tool_allowlist.clone(),
            budget_remaining: claims.budget_remaining,
            expires_unix: claims.expires_unix,
        }
    }
}
