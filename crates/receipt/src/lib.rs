//! ardur-receipt — JWS-ES256, hash-chained execution receipts.
//!
//! Plan family: §11.14 (`plans/11.14-cost-ceilings-receipts-cap-tokens-blueprint.md`)
//! plus §1.0 (the receipt-event envelope). It is split from `ardur-cap-token`
//! because the receipt signing and verification surface is consumed
//! independently — for instance by §1.12b cache-attribution receipts — without
//! ever touching cap-token issuance.
//!
//! PHASE 0: contracts only. No implementation bodies — every trait method is
//! `unimplemented!()`. The public trait surface is FROZEN against §11.14;
//! widening it is a §0.0 amendment. Bodies (the JWS-ES256 signer, the hash
//! chain) land in §11.14 Phase 1.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::Result;

/// A 32-byte chain hash linking a receipt to its parent (the previous
/// receipt's hash), forming the tamper-evident receipt chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ChainHash(pub [u8; 32]);

/// The unsigned body of a receipt: the verb under the canonical
/// `verb.object.state.v1` grammar plus the link to its parent in the chain.
#[derive(Clone, Debug)]
pub struct ReceiptBody {
    /// The receipt verb, e.g. `tool.call.completed.v1` (`verb.object.state.v1`).
    pub verb: String,
    /// Hash of the parent receipt, or `None` for the chain's genesis receipt.
    pub parent_hash: Option<ChainHash>,
    // TODO(§11.14 Phase 1): actor, cap-token jti, cost delta, timestamps.
}

/// A signed receipt — a `ReceiptBody` wrapped in a JWS-ES256 envelope. The
/// inner representation is private; construction goes through `ReceiptSigner`.
#[derive(Clone, Debug)]
pub struct SignedReceipt {
    // TODO(§11.14 Phase 1): the compact JWS string + decoded body cache.
    _private: (),
}

/// Sign a `ReceiptBody` into a `SignedReceipt` (JWS-ES256). Emits the body's
/// own verb once §11.14 lands its body.
pub trait ReceiptSigner {
    /// Sign `body`, producing a verifiable, chain-linked receipt.
    fn sign(&self, body: ReceiptBody) -> Result<SignedReceipt> {
        let _ = body;
        unimplemented!("Phase 0 contract — body lands in §11.14 Phase 1")
    }
}

/// An append-only, hash-chained receipt log. `append` links each receipt to
/// the chain head; `verify` checks the chain's integrity end-to-end.
pub trait ReceiptChain {
    /// Append a signed receipt to the chain head.
    fn append(&mut self, receipt: SignedReceipt) -> Result<()> {
        let _ = receipt;
        unimplemented!("Phase 0 contract — body lands in §11.14 Phase 1")
    }
    /// Verify the entire chain's hash linkage and signatures.
    fn verify(&self) -> Result<()> {
        unimplemented!("Phase 0 contract — body lands in §11.14 Phase 1")
    }
}
