//! The shared id and unit-bearing newtypes.
//!
//! Every type here is a thin, `#[serde(transparent)]` newtype so its wire form
//! is exactly its inner value — adopting it in place of a crate-local duplicate
//! never changes a byte on disk.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The principal a budget, cap-token, receipt, or memory record is about (a
/// runtime profile, agent, org, or session). Opaque string identifier —
/// typically a SPIFFE-style URI.
///
/// Canonical owner of the `HolderId` that `ardur-cap-token`, `ardur-cost-gate`,
/// `ardur-receipt`, and `ardur-memory` each previously defined identically.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HolderId(pub String);

impl<T: Into<String>> From<T> for HolderId {
    fn from(s: T) -> Self {
        Self(s.into())
    }
}

/// Identifier of a model provider (e.g. `"anthropic"`, `"openai"`). Opaque
/// string; screened against a provider allowlist by the cost gate.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(pub String);

/// Identifier of a concrete model within a provider (e.g.
/// `"claude-opus-4-8"`). Opaque to this layer — each provider validates it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(pub String);

impl ModelId {
    /// Wrap a model name.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Identifier of an emitted execution receipt (a UUIDv4 matching
/// `ardur_receipt::ReceiptBody::receipt_id`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReceiptId(pub Uuid);

impl ReceiptId {
    /// Mint a fresh receipt id (UUIDv4).
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ReceiptId {
    fn default() -> Self {
        Self::new()
    }
}

/// The `jti` of a capability token — its stable per-token identifier (UUIDv4),
/// as minted into `ardur_cap_token::VerifiedClaims::token_id`. This is the
/// canonical, type-safe form of a token id; the cost gate spends against it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenId(pub Uuid);

impl From<Uuid> for TokenId {
    fn from(id: Uuid) -> Self {
        Self(id)
    }
}

/// A wall-clock instant in milliseconds since the Unix epoch. `Ord` makes the
/// bi-temporal interval comparisons in `ardur-memory` total.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixTsMillis(pub u64);
