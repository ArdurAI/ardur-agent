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

/// The domain separator for the evidence-record MAC key: the emitter derives
/// its journal key from the ER signing key under this label, so the MAC key
/// is never the ECDSA key itself (cross-protocol separation).
const EVIDENCE_MAC_DOMAIN: &[u8] = b"ardur-governance/evidence-record-mac/v1";

/// Derive the evidence-journal MAC key from the ER signing key's PKCS#8 DER.
///
/// The journal is untrusted at replay: without a keyed commitment, an editor
/// of the crash window between journal append and mirror could falsify a
/// stranded record's provenance AND its publicly recomputable identity, and
/// the sweep would sign the forgery. The MAC makes the journal line
/// unforgeable without the key — the same custody as the ER chain itself.
pub fn evidence_record_mac_key(pkcs8_der: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(EVIDENCE_MAC_DOMAIN);
    h.update(pkcs8_der);
    h.finalize().into()
}

/// The HMAC-SHA256 (hex) of one canonical record line under `key`.
pub fn evidence_record_mac(key: &[u8; 32], record_line: &str) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(record_line.as_bytes());
    hex_encode(&mac.finalize().into_bytes())
}

/// Wrap one canonical record line in its authenticated journal envelope:
/// `{"mac":"<hex>","record":<json-escaped line>}`. The record bytes ride
/// verbatim inside the envelope, so replay verifies the MAC over the exact
/// bytes it then parses — no re-serialization determinism assumption.
pub fn wrap_evidence_line(key: &[u8; 32], record_line: &str) -> String {
    let mac = evidence_record_mac(key, record_line);
    let escaped = serde_json::to_string(record_line).expect("a string serializes");
    format!("{{\"mac\":\"{mac}\",\"record\":{escaped}}}")
}

/// Verify an enveloped journal line and extract the canonical record line.
///
/// # Errors
///
/// [`crate::GovernanceError::Io`] when the envelope is malformed, the MAC is
/// missing, or the MAC does not match the record bytes (tampered journal —
/// fail closed, never replay).
pub fn unwrap_evidence_line(key: &[u8; 32], line: &str) -> Result<String, crate::GovernanceError> {
    use subtle::ConstantTimeEq as _;
    let value: serde_json::Value = serde_json::from_str(line).map_err(|e| {
        crate::GovernanceError::Io(format!("evidence journal line is not a MAC envelope: {e}"))
    })?;
    let mac = value
        .get("mac")
        .and_then(|m| m.as_str())
        .ok_or_else(|| crate::GovernanceError::Io("evidence journal line lacks its MAC".into()))?;
    let record = value
        .get("record")
        .and_then(|r| r.as_str())
        .ok_or_else(|| {
            crate::GovernanceError::Io("evidence journal line lacks its record".into())
        })?;
    let expected = evidence_record_mac(key, record);
    if mac.len() != expected.len() || mac.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() != 1 {
        return Err(crate::GovernanceError::Io(
            "evidence journal MAC mismatch (tampered or corrupt journal); refusing to replay"
                .into(),
        ));
    }
    Ok(record.to_string())
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}
