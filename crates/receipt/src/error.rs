//! The crate's single typed-error surface.
//!
//! Every fallible operation in this crate returns [`ReceiptError`]. The
//! variants map one-to-one onto the §11.14 failure modes the verifier must
//! distinguish: a bad verb is not a broken chain is not a forged signature.

use crate::chain::BrokenAt;

/// All ways a receipt operation can fail.
#[derive(Debug, thiserror::Error)]
pub enum ReceiptError {
    /// A verb string did not match the canonical `verb.object.state.vN`
    /// grammar (see [`crate::VerbObject`]).
    #[error("invalid verb `{0}`: expected verb.object.state.vN")]
    InvalidVerb(String),

    /// The hash linkage between two adjacent receipts did not hold. Carries
    /// the index and the expected/actual parent hashes at the break.
    #[error("receipt chain broken at index {}", .0.at)]
    BrokenChain(BrokenAt),

    /// The JWS signature did not verify against the resolved public key.
    #[error("receipt signature did not verify")]
    SignatureInvalid,

    /// The JWS `kid` header referenced a key absent from the supplied JWKS.
    #[error("unknown key id `{0}`")]
    UnknownKid(String),

    /// The receipt was structurally invalid — not three base64url segments,
    /// undecodable bytes, or a header/payload that would not parse.
    #[error("malformed receipt: {0}")]
    Malformed(String),
}
