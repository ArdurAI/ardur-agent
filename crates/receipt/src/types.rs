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

/// Canonical verb grammar: `verb.object.state.vN`, all lowercase/underscore
/// segments, integer version suffix. E.g. `cost.admission.allow.v1`.
static VERB_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[a-z_]+\.[a-z_]+\.[a-z_]+\.v[0-9]+$").expect("valid verb regex"));

/// A 32-byte SHA-256 digest. Used both for receipt chaining (`parent_hash`,
/// the hash of the prior receipt's compact JWS) and for the opaque
/// `payload_digest`. Serializes as a lowercase 64-char hex string.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256Digest(pub [u8; 32]);

impl Sha256Digest {
    /// Compute the SHA-256 digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        let mut out = [0u8; 32];
        out.copy_from_slice(&Sha256::digest(bytes));
        Self(out)
    }

    /// Render as a lowercase 64-character hex string.
    pub fn to_hex(&self) -> String {
        use fmt::Write as _;
        let mut s = String::with_capacity(64);
        for b in &self.0 {
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    /// Parse from a lowercase/uppercase 64-character hex string.
    pub fn from_hex(s: &str) -> Result<Self, ReceiptError> {
        if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ReceiptError::Malformed(format!(
                "expected 64 hex chars, got `{s}`"
            )));
        }
        let mut out = [0u8; 32];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                .map_err(|e| ReceiptError::Malformed(e.to_string()))?;
        }
        Ok(Self(out))
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256Digest({})", self.to_hex())
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

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

/// The subject a receipt is about — the cap-token holder identity. A thin
/// newtype over a string; reconciliation with `ardur-cap-token`'s `HolderId`
/// is a §0.0 amendment, kept local here so the receipt crate stands alone.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HolderId(pub String);

impl<T: Into<String>> From<T> for HolderId {
    fn from(s: T) -> Self {
        Self(s.into())
    }
}

/// The `jti` of the capability token under whose authority the receipted
/// action occurred.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenId(pub String);

impl<T: Into<String>> From<T> for TokenId {
    fn from(s: T) -> Self {
        Self(s.into())
    }
}

/// A wall-clock instant in milliseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixTsMillis(pub u64);

/// The cost a single receipted action incurred. The D-4 "cost-as-protocol-
/// primitive" tuple: token counts, monetary cost in whole cents, wall-clock
/// duration, and an `attention_score` (the share of human attention the
/// action consumed, `0.0..=1.0`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CostTuple {
    /// Prompt/input tokens billed.
    pub tokens_in: u64,
    /// Completion/output tokens billed.
    pub tokens_out: u64,
    /// Monetary cost in whole US cents.
    pub cents: u64,
    /// Wall-clock duration of the action, in milliseconds.
    pub wall_ms: u64,
    /// Share of human attention consumed, conventionally `0.0..=1.0`.
    pub attention_score: f64,
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
