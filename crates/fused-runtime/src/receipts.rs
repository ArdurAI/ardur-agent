//! Durable receipt-chain persistence and cross-restart linkage verification.
//!
//! The fused runtime appends each turn's signed receipt to an append-only log,
//! one compact JWS per line (the receipt crate names the JWS "the canonical,
//! hashed-over form used for chaining"). A fresh runtime over the same path
//! resumes the chain from the last line's hash, and
//! [`verify_persisted_chain`] re-checks the whole chain off disk.
//!
//! Why we re-implement the linkage check instead of calling
//! [`ardur_receipt::verify_chain`]: that function takes
//! `&[SignedReceipt]`, and a `SignedReceipt` cannot be reconstructed outside the
//! receipt crate (its `from_parts` constructor is crate-private). So we persist
//! the JWS, decode each one's body for its `parent_hash`, and apply the *same*
//! rule `verify_chain` does — `parent_hash[i] == SHA256(jws[i-1])`, genesis
//! carries `None`.

use std::path::Path;

use ardur_receipt::{ReceiptBody, Sha256Digest};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;

/// One receipt as persisted on disk: its canonical compact JWS plus the
/// [`ReceiptBody`] decoded from that JWS's payload segment.
#[derive(Clone, Debug)]
pub struct PersistedReceipt {
    /// The compact JWS string (`header.payload.sig`) — the bytes a child
    /// receipt's `parent_hash` is the SHA-256 of.
    pub jws_compact: String,
    /// The body decoded from the JWS payload segment.
    pub body: ReceiptBody,
}

/// A failure loading or verifying a persisted receipt chain.
#[derive(Debug, thiserror::Error)]
pub enum ReceiptChainError {
    /// The receipt-log file could not be read.
    #[error("receipt log i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// A line was not a well-formed compact JWS, or its payload did not decode
    /// to a [`ReceiptBody`].
    #[error("malformed persisted receipt: {0}")]
    Malformed(String),
    /// The hash linkage broke at receipt index `at`: its `parent_hash` did not
    /// equal the SHA-256 of the previous receipt's JWS (or the genesis receipt
    /// carried a non-`None` parent).
    #[error("broken receipt chain at index {at}")]
    BrokenChain {
        /// Index into the loaded chain at which the mismatch was found.
        at: usize,
    },
}

/// Decode the [`ReceiptBody`] out of a compact JWS's payload (middle) segment.
fn decode_body(jws_compact: &str) -> Result<ReceiptBody, ReceiptChainError> {
    let payload_b64 = jws_compact
        .split('.')
        .nth(1)
        .ok_or_else(|| ReceiptChainError::Malformed("not three JWS segments".to_string()))?;
    let payload = B64URL
        .decode(payload_b64)
        .map_err(|e| ReceiptChainError::Malformed(format!("payload base64url: {e}")))?;
    serde_json::from_slice(&payload)
        .map_err(|e| ReceiptChainError::Malformed(format!("payload json: {e}")))
}

/// Load every persisted receipt from `path`, in append (chain) order. A missing
/// file is an empty chain (no turns have been receipted yet).
pub fn load_persisted_chain(
    path: impl AsRef<Path>,
) -> Result<Vec<PersistedReceipt>, ReceiptChainError> {
    let raw = match std::fs::read_to_string(path.as_ref()) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(ReceiptChainError::Io(e)),
    };
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let body = decode_body(line)?;
            Ok(PersistedReceipt {
                jws_compact: line.to_string(),
                body,
            })
        })
        .collect()
}

/// Verify the hash linkage of a loaded chain: the first receipt must be a
/// genesis (`parent_hash == None`) and every later receipt's `parent_hash` must
/// equal `SHA256` of the previous receipt's JWS. Returns the index of the first
/// break, mirroring [`ardur_receipt::verify_chain`]'s rule over on-disk data.
pub fn verify_persisted_chain(chain: &[PersistedReceipt]) -> Result<(), ReceiptChainError> {
    let mut prev: Option<&PersistedReceipt> = None;
    for (at, receipt) in chain.iter().enumerate() {
        let expected = prev.map(|p| Sha256Digest::of(p.jws_compact.as_bytes()));
        if receipt.body.parent_hash != expected {
            return Err(ReceiptChainError::BrokenChain { at });
        }
        prev = Some(receipt);
    }
    Ok(())
}
