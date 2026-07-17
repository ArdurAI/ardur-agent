//! The canonical SHA-256 digest type.
//!
//! Before consolidation the workspace carried three incompatible digest wire
//! forms: `ardur-receipt` serialized lowercase hex, `ardur-cost-gate` derived
//! the default `[u8; 32]` encoding (a 32-element JSON number array), and
//! `ardur-session-journals` stored a bare hex `String`. Because digests feed
//! signed, hash-chained receipts, they must have exactly one wire form — this
//! type fixes it to **lowercase hex** and stores the raw 32 bytes.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A hex-parse failure. Kept crate-local so `ardur-core-types` stays leaf; the
/// crates that surface a richer error convert from this via `From`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestParseError(String);

impl DigestParseError {
    /// The human-readable reason the hex string was rejected.
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DigestParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DigestParseError {}

/// A 32-byte SHA-256 digest. Used for receipt chaining (`parent_hash`), opaque
/// `payload_digest`s, request binding in the cost gate, and tool-payload
/// digests in the session journal. Serializes as a lowercase 64-char hex
/// string.
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

    /// Parse from a 64-character hex string (either case).
    ///
    /// # Errors
    ///
    /// Returns [`DigestParseError`] if `s` is not exactly 64 hex characters.
    pub fn from_hex(s: &str) -> Result<Self, DigestParseError> {
        if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(DigestParseError(format!(
                "expected 64 hex chars, got `{s}`"
            )));
        }
        let mut out = [0u8; 32];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                .map_err(|e| DigestParseError(e.to_string()))?;
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
