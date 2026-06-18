//! Public value types for cap-token issuance, attenuation, and verification.

use biscuit_auth::{Biscuit, PublicKey};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{CapTokenError, map_parse_error};

/// The principal a cap-token is issued to (a runtime profile, agent, or
/// session). Opaque string identifier — typically a SPIFFE-style URI.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HolderId(pub String);

/// The capability scope a cap-token grants: the audience it is valid for, when
/// it expires, the spend ceiling, the tools it may invoke, and the
/// capability-level permissions it grants. These become the authority block's
/// claims and the checks that bind every future request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapScope {
    /// The single audience (service / boundary) the token authorizes.
    pub audience: String,
    /// Expiry as a Unix timestamp in seconds. A request whose time is strictly
    /// greater than this is [`CapTokenError::Expired`].
    pub expires_unix: u64,
    /// The spend ceiling. A request whose declared cost exceeds this is
    /// [`CapTokenError::BudgetExhausted`].
    pub budget_remaining: u64,
    /// The tools this token may invoke. A request for a tool outside this set
    /// is [`CapTokenError::ToolNotAllowed`]. An empty allowlist denies all
    /// tools.
    pub tool_allowlist: Vec<String>,
    /// The capabilities this token grants, as their canonical string forms.
    /// The fused runtime checks a tool's `required_capabilities` against this
    /// set before every invocation (ARD-420). Empty means no tool may declare
    /// a non-empty `required_capabilities`.
    pub capabilities: Vec<String>,
}

/// A single strictly-narrowing rule applied during attenuation. Each variant
/// appends one Biscuit check; because checks only ever intersect authority, a
/// rule can shrink a capability but never widen it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttenuationRule {
    /// Pin the token to a specific audience (intersected with the issued one).
    RestrictAudience(String),
    /// Bring the expiry forward to this Unix-seconds timestamp.
    EarlierExpiry(u64),
    /// Lower the spend ceiling to this many budget units.
    ReduceBudget(u64),
    /// Shrink the tool allowlist to this subset.
    RestrictTools(Vec<String>),
}

/// A strictly-narrowing caveat applied during attenuation. Widening a
/// capability is unrepresentable by construction: a caveat only ever appends a
/// check, and the token's effective authority is the intersection of all
/// blocks' checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caveat {
    /// The narrowing rule this caveat applies.
    pub rule: AttenuationRule,
}

impl Caveat {
    /// Wrap a narrowing [`AttenuationRule`] in a caveat.
    pub fn new(rule: AttenuationRule) -> Self {
        Self { rule }
    }
}

impl From<AttenuationRule> for Caveat {
    fn from(rule: AttenuationRule) -> Self {
        Self { rule }
    }
}

/// An opaque, signed cap-token. Wraps a Biscuit; the raw token is exposed only
/// through the serialization helpers below and the [`Biscuit`] re-export.
#[derive(Clone, Debug)]
pub struct CapToken(pub Biscuit);

impl CapToken {
    /// Serialize to the canonical URL-safe base64 wire form.
    pub fn to_base64(&self) -> Result<String, CapTokenError> {
        self.0
            .to_base64()
            .map_err(|e| CapTokenError::Malformed(e.to_string()))
    }

    /// Parse a base64 token and verify its block signatures against `root`.
    pub fn from_base64(token: &str, root: &PublicKey) -> Result<Self, CapTokenError> {
        Biscuit::from_base64(token, *root)
            .map(CapToken)
            .map_err(map_parse_error)
    }

    /// Serialize to the canonical protobuf wire bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>, CapTokenError> {
        self.0
            .to_vec()
            .map_err(|e| CapTokenError::Malformed(e.to_string()))
    }

    /// Parse token bytes and verify their block signatures against `root`.
    pub fn from_bytes(bytes: &[u8], root: &PublicKey) -> Result<Self, CapTokenError> {
        Biscuit::from(bytes, *root)
            .map(CapToken)
            .map_err(map_parse_error)
    }

    /// The Biscuit revocation identifiers for every block, in order. A token is
    /// revoked if a [`DenyList`](crate::DenyList) holds any of these.
    pub fn revocation_ids(&self) -> Vec<Vec<u8>> {
        self.0.revocation_identifiers()
    }
}

/// What the verifier asserts about the concrete request a token is being
/// checked against. Every field is always supplied as a Biscuit fact so the
/// token's checks (and any attenuation's) have the bindings they reference.
#[derive(Clone, Debug)]
pub struct RequiredCaveats {
    /// Current time as Unix seconds — checked against the token's expiry.
    pub now_unix: u64,
    /// The audience presenting the token.
    pub audience: String,
    /// The tool the request will invoke.
    pub tool: String,
    /// The budget units this request will consume.
    pub cost: u64,
}

/// The claims carried by a verified token, returned once every caveat (the
/// authority block's and every attenuation's) has been satisfied by the
/// request. These are the *issued* claims read from the authority block; the
/// effective authority a request must satisfy may be narrower (each attenuation
/// further constrains it), which is why verification enforces the caveats
/// rather than re-deriving them from these claims.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedClaims {
    /// Stable per-token identifier (UUIDv4), suitable as a receipt's
    /// `cap_token_id`.
    pub token_id: Uuid,
    /// The issued audience.
    pub audience: String,
    /// The principal the token is bound to.
    pub subject: HolderId,
    /// The issued expiry, as Unix seconds.
    pub expires_unix: u64,
    /// The issued spend ceiling.
    pub budget_remaining: u64,
    /// The issued tool allowlist.
    pub tool_allowlist: Vec<String>,
    /// The issued capability grants, as canonical capability strings.
    pub capabilities: Vec<String>,
}

impl VerifiedClaims {
    /// Whether the token grants the given capability string. Used by the fused
    /// runtime (ARD-420) to enforce a tool's `required_capabilities` before
    /// every invocation. The string is the canonical form a tool's
    /// `required_capabilities` produces (e.g. `Capability::as_str()`).
    #[must_use]
    pub fn has_capability(&self, cap: &str) -> bool {
        self.capabilities.iter().any(|c| c == cap)
    }
}

/// The issued claims, serialized into the authority block's context so the
/// verifier can return them after authorization. The block is signed, so this
/// payload is tamper-evident; it is never the source of truth for enforcement
/// (the checks are).
#[derive(Serialize, Deserialize)]
pub(crate) struct CapClaims {
    pub token_id: Uuid,
    pub audience: String,
    pub subject: String,
    pub expires_unix: u64,
    pub budget_remaining: u64,
    pub tool_allowlist: Vec<String>,
    pub capabilities: Vec<String>,
}
