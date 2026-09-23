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

/// Envelope one canonical record line in its chained journal envelope:
/// `{"mac":"<hex>","record":<json-escaped line>,"seq":<n>}`. The chain-MAC of
/// line `seq` covers the previous line's chain MAC (`prev_chain_mac`, empty
/// for the first line), so deleting any line breaks the linkage and
/// truncating the tail contradicts the authenticated checkpoint. The record
/// bytes ride verbatim inside the envelope, so replay verifies the MAC over
/// the exact bytes it then parses — no re-serialization determinism
/// assumption. Returns the envelope and its chain MAC (the next line's
/// `prev_chain_mac`).
pub fn wrap_evidence_line(
    key: &[u8; 32],
    seq: u64,
    prev_chain_mac: &str,
    record_line: &str,
) -> (String, String) {
    let mac = evidence_line_chain_mac(key, seq, prev_chain_mac, record_line);
    let escaped = serde_json::to_string(record_line).expect("a string serializes");
    (
        format!("{{\"mac\":\"{mac}\",\"record\":{escaped},\"seq\":{seq}}}"),
        mac,
    )
}

/// Verify an enveloped journal line and extract the canonical record line
/// plus the verified chain MAC (the next line's `prev_chain_mac`). The caller
/// walks the journal in order, threading the expected sequence number and the
/// previous line's chain MAC through each call.
///
/// # Errors
///
/// [`crate::GovernanceError::Io`] when the envelope is malformed, the
/// sequence breaks the chain (a deleted line), or the MAC does not match
/// (tampered journal — fail closed, never replay).
pub fn unwrap_evidence_line(
    key: &[u8; 32],
    seq: u64,
    prev_chain_mac: &str,
    line: &str,
) -> Result<(String, String), crate::GovernanceError> {
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
    let presented_seq = value.get("seq").and_then(|s| s.as_u64()).ok_or_else(|| {
        crate::GovernanceError::Io("evidence journal line lacks its sequence number".into())
    })?;
    if presented_seq != seq {
        return Err(crate::GovernanceError::Io(format!(
            "evidence journal seq {presented_seq} does not continue the chain (expected {seq}); a line may have been deleted"
        )));
    }
    let expected = evidence_line_chain_mac(key, seq, prev_chain_mac, record);
    if mac.len() != expected.len() || mac.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() != 1 {
        return Err(crate::GovernanceError::Io(
            "evidence journal MAC mismatch (record tampered or a line deleted); refusing to replay"
                .into(),
        ));
    }
    // The verified chain MAC (constant-time equal to the presented one) is
    // what the NEXT line's MAC must cover.
    Ok((record.to_string(), expected))
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

/// The chain-MAC domain label for one evidence journal line. Each line's
/// MAC covers its sequence number and the PREVIOUS line's chain MAC, so a
/// deletion anywhere in the journal breaks the linkage — per-line MACs
/// alone authenticate each line independently and cannot detect one going
/// missing.
const EVIDENCE_LINE_DOMAIN: &[u8] = b"ardur-governance/evidence-line/v1";

/// The checkpoint domain label: the authenticated tail commitment the
/// emitter rewrites after every journal append, so truncating the journal's
/// tail is as detectable as a mid-journal deletion.
const EVIDENCE_CHECKPOINT_DOMAIN: &[u8] = b"ardur-governance/evidence-checkpoint/v1";

fn hmac_sha256(key: &[u8; 32], parts: &[&[u8]]) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key length");
    for part in parts {
        mac.update(part);
    }
    hex_encode(&mac.finalize().into_bytes())
}

/// The chain-MAC of journal line `seq` over `record_line`, linked to the
/// previous line's chain MAC (`prev_chain_mac`, empty for the first line).
pub fn evidence_line_chain_mac(
    key: &[u8; 32],
    seq: u64,
    prev_chain_mac: &str,
    record_line: &str,
) -> String {
    hmac_sha256(
        key,
        &[
            EVIDENCE_LINE_DOMAIN,
            &seq.to_be_bytes(),
            prev_chain_mac.as_bytes(),
            record_line.as_bytes(),
        ],
    )
}

/// The authenticated tail checkpoint of a journal whose last line is `seq`
/// with chain MAC `tail_chain_mac`: a small JSON document, rewritten after
/// every append with the same fsync discipline as the journal itself.
pub fn evidence_checkpoint_json(key: &[u8; 32], seq: u64, tail_chain_mac: &str) -> String {
    let checkpoint_mac = hmac_sha256(
        key,
        &[
            EVIDENCE_CHECKPOINT_DOMAIN,
            &seq.to_be_bytes(),
            tail_chain_mac.as_bytes(),
        ],
    );
    format!(
        "{{\"checkpoint_mac\":\"{checkpoint_mac}\",\"seq\":{seq},\"tail_mac\":\"{tail_chain_mac}\"}}"
    )
}

/// Verify a checkpoint document and return its (seq, tail chain MAC).
///
/// # Errors
///
/// [`crate::GovernanceError::Io`] when the document is malformed or its MAC
/// does not verify (tail truncation or tampering — fail closed).
pub fn verify_evidence_checkpoint(
    key: &[u8; 32],
    json: &str,
) -> Result<(u64, String), crate::GovernanceError> {
    use subtle::ConstantTimeEq as _;
    let value: serde_json::Value = serde_json::from_str(json).map_err(|e| {
        crate::GovernanceError::Io(format!("evidence checkpoint is not valid JSON: {e}"))
    })?;
    let seq = value
        .get("seq")
        .and_then(|s| s.as_u64())
        .ok_or_else(|| crate::GovernanceError::Io("evidence checkpoint lacks seq".into()))?;
    let tail_mac = value
        .get("tail_mac")
        .and_then(|m| m.as_str())
        .ok_or_else(|| crate::GovernanceError::Io("evidence checkpoint lacks tail_mac".into()))?;
    let presented = value
        .get("checkpoint_mac")
        .and_then(|m| m.as_str())
        .ok_or_else(|| {
            crate::GovernanceError::Io("evidence checkpoint lacks checkpoint_mac".into())
        })?;
    let expected = hmac_sha256(
        key,
        &[
            EVIDENCE_CHECKPOINT_DOMAIN,
            &seq.to_be_bytes(),
            tail_mac.as_bytes(),
        ],
    );
    if presented.len() != expected.len()
        || presented.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() != 1
    {
        return Err(crate::GovernanceError::Io(
            "evidence checkpoint MAC mismatch (tail truncation or tampering); refusing to replay"
                .into(),
        ));
    }
    Ok((seq, tail_mac.to_string()))
}
