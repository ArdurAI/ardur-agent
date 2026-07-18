//! SHA-256 helpers shared by projection (argument/invocation digests) and
//! signing (the `parent_receipt_hash` chain link).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use sha2::{Digest, Sha256};

/// Raw 32-byte SHA-256 of `bytes`.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Lowercase hex SHA-256 of `bytes` (64 chars) — the ER `arguments_hash` /
/// `parent_receipt_hash` form.
pub fn sha256_hex(bytes: &[u8]) -> String {
    to_hex(&sha256(bytes))
}

/// Base64url (no pad) SHA-256 of `bytes` — the ER `digestObject.value` form.
pub fn sha256_b64url(bytes: &[u8]) -> String {
    B64URL.encode(sha256(bytes))
}

/// Lowercase hex encoding of a byte slice.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
