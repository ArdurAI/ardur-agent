//! The value types that make up an unsigned receipt body.
//!
//! These are deliberately transport-agnostic: a [`ReceiptBody`] is just data.
//! Signing ([`crate::ReceiptSigner`]) and chaining ([`crate::ReceiptChain`])
//! are layered on top so the body can be reused by non-cost receipt families
//! (e.g. §1.12b cache-attribution) without dragging in the JWS surface.

use std::fmt;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::error::ReceiptError;

// The cost tuple, holder id, unix-millis instant, and SHA-256 digest are owned
// by `ardur-core-types` and re-exported here so `ardur_receipt::{CostTuple,
// HolderId, UnixTsMillis, Sha256Digest}` — and the receipt body's own fields —
// resolve to the one canonical type. The digest's canonical wire form is
// lowercase hex, exactly as this crate always serialized it.
pub use ardur_core_types::{CostTuple, HolderId, Sha256Digest, UnixTsMillis};

/// Canonical verb grammar: `verb.object.state.vN`, all lowercase/underscore
/// segments, integer version suffix. E.g. `cost.admission.allow.v1`.
static VERB_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[a-z_]+\.[a-z_]+\.[a-z_]+\.v[0-9]+$").expect("valid verb regex"));

/// A receipt verb validated against the canonical `verb.object.state.vN`
/// grammar. Construction is the only way the invariant is established, so a
/// `VerbObject` value is always well-formed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct VerbObject(String);

impl VerbObject {
    /// Validate and wrap a verb string. Returns [`ReceiptError::InvalidVerb`]
    /// if it does not match `^[a-z_]+\.[a-z_]+\.[a-z_]+\.v[0-9]+$`.
    pub fn new(verb: impl Into<String>) -> Result<Self, ReceiptError> {
        let verb = verb.into();
        if VERB_RE.is_match(&verb) {
            Ok(Self(verb))
        } else {
            Err(ReceiptError::InvalidVerb(verb))
        }
    }

    /// Borrow the underlying verb string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VerbObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for VerbObject {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for VerbObject {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::new(s).map_err(serde::de::Error::custom)
    }
}

/// The `jti` of the capability token under whose authority the receipted
/// action occurred.
///
// NOTE §0.0 reconciliation: the canonical, type-safe token id is
// [`ardur_core_types::TokenId`] (a UUIDv4, matching a cap-token's minted
// `jti`). This receipt-local form stays a free-form `String` because a receipt
// subject's `cap_token_id` is today populated from heterogeneous producers —
// a stringified cap-token UUID at the fused-runtime mint, but an opaque
// `CapTokenRef` handle from the Phase-1 hooked runtime. Migrating to the Uuid
// newtype is a behavioural change on those producers, tracked as a follow-up.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenId(pub String);

impl<T: Into<String>> From<T> for TokenId {
    fn from(s: T) -> Self {
        Self(s.into())
    }
}

/// One tool invocation a turn made, recorded on its [`ReceiptBody`] for audit.
///
/// The tool's arguments and output are not carried inline — only their SHA-256
/// digests, mirroring [`ReceiptBody::payload_digest`]'s tamper-evidence posture
/// — alongside the cost the call billed, so a turn's tool spend reconciles
/// against its receipted total (§6.0).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCallReceipt {
    /// The provider-assigned id of the call this records.
    pub call_id: String,
    /// The tool that was invoked.
    pub tool_name: String,
    /// SHA-256 of the JSON arguments the model passed.
    pub arguments_digest: Sha256Digest,
    /// SHA-256 of the JSON output the tool returned.
    pub output_digest: Sha256Digest,
    /// Cost the invocation billed (folded into the turn's total [`cost`]).
    ///
    /// [`cost`]: ReceiptBody::cost
    pub cost: CostTuple,
}

/// The unsigned body of a receipt — the canonical payload that gets signed
/// into a [`crate::SignedReceipt`] and hash-chained into a receipt log.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReceiptBody {
    /// Stable, unique receipt identifier (UUIDv4).
    pub receipt_id: Uuid,
    /// SHA-256 of the prior receipt's compact JWS, or `None` for a genesis
    /// receipt. Set by [`crate::ReceiptChain::append`].
    pub parent_hash: Option<Sha256Digest>,
    /// The receipt verb, e.g. `cost.admission.allow.v1`.
    pub verb: VerbObject,
    /// When the receipted action occurred.
    pub issued_at: UnixTsMillis,
    /// The holder the receipt is about.
    pub subject: HolderId,
    /// The capability token under which the action ran.
    pub cap_token_id: TokenId,
    /// Opaque digest of the event-specific payload (the payload itself is not
    /// carried in the body — only its hash, for tamper-evidence).
    pub payload_digest: Sha256Digest,
    /// Durable journal identity that owns this receipt. Additive for backward
    /// compatibility; legacy receipts without this field cannot be assigned to
    /// a per-session reconciliation sweep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    /// Cost incurred by the action.
    pub cost: CostTuple,
    /// The tool calls this turn made, if any (§6.0). Additive: `#[serde(default)]`
    /// so receipts written before this field load with an empty list, and
    /// `skip_serializing_if` keeps a no-tool receipt's bytes (and therefore its
    /// signature and chain hash) identical to a pre-§6.0 one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallReceipt>,
    /// The model backend that served this turn (§11.14b), e.g. `"anthropic"`.
    /// Populated at the fused-runtime mint from [`Provider::name`]. Additive:
    /// `#[serde(default)]` so receipts written before this field load with
    /// `None`, and `skip_serializing_if` keeps a `None` receipt's bytes (and
    /// therefore its signature and chain hash) byte-identical to a pre-§11.14b
    /// one.
    ///
    /// [`Provider::name`]: https://docs.rs/ardur-provider-runtime
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}
