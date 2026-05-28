//! ardur-receipt — JWS-ES256, hash-chained execution receipts.
//!
//! Plan family: §11.14
//! (`plans/11.14-cost-ceilings-receipts-cap-tokens-blueprint.md`) plus §1.0
//! (the receipt-event envelope). Design record: ADR-Phase3-549 (JWS over raw
//! `p256` rather than a JWT facade, so the verifier can publish and consume
//! JWKS `x`/`y` affine coordinates and reconstruct verifying keys offline).
//! It is split from `ardur-cap-token` because the receipt signing/verification
//! surface is consumed independently — e.g. by §1.12b cache-attribution
//! receipts — without ever touching cap-token issuance.
//!
//! # Phase 1 (this crate)
//!
//! - [`ReceiptBody`] — the canonical unsigned body, with a [`VerbObject`]
//!   constrained to the `verb.object.state.vN` grammar.
//! - [`ReceiptSigner`] / [`ReceiptVerifier`] — JWS Compact (`ES256`,
//!   `typ=ardur-receipt+jws`) signing and `kid`-resolved verification.
//! - [`ReceiptChain`] / [`verify_chain`] — `parent_hash` linkage where each
//!   hash is `SHA256` of the prior receipt's compact JWS.
//! - [`Es256SigningKey`] / [`Es256PublicKey`] / [`Jwks`] — P-256 key custody
//!   (PKCS#8 PEM) and the JWKS publication format.
//!
//! Phase 2 (see inline `// TODO §11.14 Phase 2:` markers) adds the Merkle
//! batch witness and JWKS rotation.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod chain;
mod error;
mod jws;
mod keys;
mod types;

pub use chain::{BrokenAt, ReceiptChain, verify_chain};
pub use error::ReceiptError;
pub use jws::{ReceiptSigner, ReceiptVerifier, SignedReceipt, VerifiedReceipt};
pub use keys::{Es256PublicKey, Es256SigningKey, Jwks, JwksKey};
pub use types::{
    CostTuple, HolderId, ReceiptBody, Sha256Digest, TokenId, UnixTsMillis, VerbObject,
};
