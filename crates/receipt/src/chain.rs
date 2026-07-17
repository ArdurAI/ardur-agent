//! The tamper-evident hash chain over signed receipts.
//!
//! Each receipt's `parent_hash` is `SHA256` of the prior receipt's compact
//! JWS. Because the JWS covers the entire body (including the *parent's* own
//! `parent_hash`), mutating any receipt invalidates every link after it.

use crate::error::ReceiptError;
use crate::jws::{ReceiptVerifier, SignedReceipt};
use crate::keys::Jwks;
use crate::types::{ReceiptBody, Sha256Digest};

/// Where, and how, a receipt chain's hash linkage first fails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrokenAt {
    /// Index into the verified slice at which the mismatch was found.
    pub at: usize,
    /// The parent hash the chain required at this index (the SHA-256 of the
    /// previous receipt's JWS, or `None` at the genesis position).
    pub expected: Option<Sha256Digest>,
    /// The parent hash the receipt actually carried.
    pub actual: Option<Sha256Digest>,
}

/// Links receipt bodies into the chain.
pub struct ReceiptChain;

impl ReceiptChain {
    /// Return `body` with its `parent_hash` set to `SHA256(prior_tail's JWS)`,
    /// or `None` when `prior_tail` is `None` (the chain's genesis receipt).
    pub fn append(prior_tail: Option<&SignedReceipt>, mut body: ReceiptBody) -> ReceiptBody {
        body.parent_hash = prior_tail.map(|tail| Sha256Digest::of(tail.jws_compact().as_bytes()));
        body
    }
}

/// Verify the integrity of an ordered slice of signed receipts.
///
/// Returns `Ok(())` when, for every receipt:
/// 1. its JWS ES256 signature verifies against the public key resolved from
///    `jwks` via its `kid` header (ARD-479 — previously only the hash linkage
///    was checked, so a forged/absent signature passed if the hash linked), and
/// 2. its `parent_hash` equals the SHA-256 of the previous receipt's compact
///    JWS (and the first receipt is a genesis, `parent_hash == None`).
///
/// Signature verification runs *before* the linkage check, so a receipt with a
/// forged signature is rejected even if its stored `parent_hash` happens to
/// link. On the first signature failure the underlying
/// [`ReceiptError::SignatureInvalid`] / [`ReceiptError::UnknownKid`] /
/// [`ReceiptError::Malformed`] is returned; on the first linkage mismatch
/// [`ReceiptError::BrokenChain`] carrying the [`BrokenAt`] location is returned.
///
// TODO §11.14 Phase 2: batch the verified hashes into a binary Merkle tree
// and check the recomputed root against the Authority-published root.
pub fn verify_chain(receipts: &[SignedReceipt], jwks: &Jwks) -> Result<(), ReceiptError> {
    let mut prev: Option<&SignedReceipt> = None;
    for (at, receipt) in receipts.iter().enumerate() {
        // ARD-479: verify the JWS ES256 signature before trusting any derived
        // data (including this receipt's parent_hash linkage).
        ReceiptVerifier::verify(receipt, jwks)?;
        let expected = prev.map(|p| Sha256Digest::of(p.jws_compact().as_bytes()));
        let actual = receipt.body().parent_hash;
        if expected != actual {
            return Err(ReceiptError::BrokenChain(BrokenAt {
                at,
                expected,
                actual,
            }));
        }
        prev = Some(receipt);
    }
    Ok(())
}
