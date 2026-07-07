//! JWS Compact signing and verification of receipt bodies.
//!
//! The wire format is a standard JWS Compact Serialization
//! (`base64url(header).base64url(payload).base64url(sig)`) with a fixed
//! protected header: `alg=ES256`, `typ=ardur-receipt+jws`, and the signer's
//! `kid`. The signature is IEEE P1363 fixed-width (R‖S), per RFC 7518 §3.4.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use p256::ecdsa::Signature;
use p256::ecdsa::signature::{Signer as _, Verifier as _};
use serde::{Deserialize, Serialize};

use crate::error::ReceiptError;
use crate::keys::{Es256PublicKey, Es256SigningKey, Jwks};
use crate::types::ReceiptBody;

/// JWS `alg` for ES256 receipts.
const ALG: &str = "ES256";
/// JWS `typ` identifying an Ardur receipt.
const TYP: &str = "ardur-receipt+jws";

/// The JWS protected header.
#[derive(Serialize, Deserialize)]
struct JwsHeader {
    alg: String,
    kid: String,
    typ: String,
}

/// A receipt body wrapped in a verifiable JWS-ES256 envelope. The compact JWS
/// string is the canonical, hashed-over form used for chaining.
#[derive(Clone, Debug, PartialEq)]
pub struct SignedReceipt {
    jws_compact: String,
    body: ReceiptBody,
}

impl SignedReceipt {
    /// The compact JWS string (`header.payload.sig`). This exact byte sequence
    /// is what a child receipt's `parent_hash` is computed over.
    pub fn jws_compact(&self) -> &str {
        &self.jws_compact
    }

    /// The decoded body carried by this receipt.
    pub fn body(&self) -> &ReceiptBody {
        &self.body
    }

    pub(crate) fn from_parts(jws_compact: String, body: ReceiptBody) -> Self {
        Self { jws_compact, body }
    }
}

/// Signs receipt bodies into [`SignedReceipt`]s.
pub struct ReceiptSigner;

impl ReceiptSigner {
    /// Sign `body` with `key`, producing a JWS-ES256 compact receipt.
    pub fn sign(body: ReceiptBody, key: &Es256SigningKey) -> Result<SignedReceipt, ReceiptError> {
        let header = JwsHeader {
            alg: ALG.to_string(),
            kid: key.key_id(),
            typ: TYP.to_string(),
        };
        let header_json =
            serde_json::to_vec(&header).map_err(|e| ReceiptError::Malformed(e.to_string()))?;
        let payload_json =
            serde_json::to_vec(&body).map_err(|e| ReceiptError::Malformed(e.to_string()))?;
        let signing_input = format!(
            "{}.{}",
            B64URL.encode(header_json),
            B64URL.encode(payload_json)
        );

        let sig: Signature = key
            .inner()
            .try_sign(signing_input.as_bytes())
            .map_err(|e| ReceiptError::Malformed(format!("sign: {e}")))?;
        // ARD-483: emit canonical low-S so the verifier (which rejects high-S)
        // accepts our own receipts.
        let sig = sig.normalize_s().unwrap_or(sig);
        let jws_compact = format!("{signing_input}.{}", B64URL.encode(sig.to_bytes()));

        Ok(SignedReceipt::from_parts(jws_compact, body))
    }
}

/// A receipt whose signature has been checked against a known public key.
#[derive(Clone, Debug, PartialEq)]
pub struct VerifiedReceipt {
    /// The verified body.
    pub body: ReceiptBody,
    /// The `kid` whose key verified the signature.
    pub kid: String,
}

/// Verifies [`SignedReceipt`]s against a [`Jwks`].
pub struct ReceiptVerifier;

impl ReceiptVerifier {
    /// Verify `receipt`: resolve its `kid` in `jwks`, check the ES256
    /// signature, and decode the body.
    pub fn verify(receipt: &SignedReceipt, jwks: &Jwks) -> Result<VerifiedReceipt, ReceiptError> {
        let mut parts = receipt.jws_compact().split('.');
        let (header_b64, payload_b64, sig_b64) =
            match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some(h), Some(p), Some(s), None) => (h, p, s),
                _ => {
                    return Err(ReceiptError::Malformed(
                        "expected three JWS Compact segments".to_string(),
                    ));
                }
            };

        let header_bytes = B64URL
            .decode(header_b64)
            .map_err(|e| ReceiptError::Malformed(format!("header base64url: {e}")))?;
        let header: JwsHeader = serde_json::from_slice(&header_bytes)
            .map_err(|e| ReceiptError::Malformed(format!("header json: {e}")))?;
        if header.alg != ALG {
            return Err(ReceiptError::Malformed(format!(
                "unsupported alg `{}`",
                header.alg
            )));
        }

        let jwk = jwks
            .lookup(&header.kid)
            .ok_or_else(|| ReceiptError::UnknownKid(header.kid.clone()))?;
        let pubkey = Es256PublicKey::from_jwk(jwk)?;

        let sig_bytes = B64URL
            .decode(sig_b64)
            .map_err(|_| ReceiptError::SignatureInvalid)?;
        let sig = Signature::from_slice(&sig_bytes).map_err(|_| ReceiptError::SignatureInvalid)?;
        // ARD-483: enforce canonical low-S (BIP-62). ECDSA admits a second valid
        // signature (n - s) for the same key+message; rejecting s > n/2 removes
        // the malleable twin and pins one signature per (key, message).
        if sig.normalize_s().is_some() {
            return Err(ReceiptError::SignatureInvalid);
        }
        let signing_input = format!("{header_b64}.{payload_b64}");
        pubkey
            .inner()
            .verify(signing_input.as_bytes(), &sig)
            .map_err(|_| ReceiptError::SignatureInvalid)?;

        let payload_bytes = B64URL
            .decode(payload_b64)
            .map_err(|e| ReceiptError::Malformed(format!("payload base64url: {e}")))?;
        let body: ReceiptBody = serde_json::from_slice(&payload_bytes)
            .map_err(|e| ReceiptError::Malformed(format!("payload json: {e}")))?;

        Ok(VerifiedReceipt {
            body,
            kid: header.kid,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CostTuple, HolderId, Sha256Digest, TokenId, UnixTsMillis, VerbObject};

    fn sample_body() -> ReceiptBody {
        ReceiptBody {
            receipt_id: uuid::Uuid::new_v4(),
            parent_hash: None,
            verb: VerbObject::new("cost.admission.allow.v1").unwrap(),
            issued_at: UnixTsMillis(1_700_000_000_000),
            subject: HolderId("spiffe://ardur/user/alice".to_string()),
            cap_token_id: TokenId("jti-0001".to_string()),
            payload_digest: Sha256Digest::of(b"event-payload"),
            cost: CostTuple {
                tokens_in: 100,
                tokens_out: 50,
                cents: 2,
                wall_ms: 1_200,
                attention_score: 0.5,
            },
            tool_calls: Vec::new(),
            provider: None,
        }
    }

    /// ARD-483: signing emits canonical low-S, so the high-S-rejecting verifier
    /// accepts our own receipts.
    #[test]
    fn sign_emits_canonical_low_s() {
        let key = Es256SigningKey::generate();
        let jwks = Jwks::from_public_key(&key.public_key());
        let signed = ReceiptSigner::sign(sample_body(), &key).unwrap();
        let sig_b64 = signed.jws_compact().split('.').nth(2).unwrap();
        let sig = Signature::from_slice(&B64URL.decode(sig_b64).unwrap()).unwrap();
        assert!(sig.normalize_s().is_none(), "signer must emit low-S");
        assert!(ReceiptVerifier::verify(&signed, &jwks).is_ok());
    }

    /// ARD-483: a malleable high-S twin of a valid signature is rejected, even
    /// though it is mathematically valid for the same key+message.
    #[test]
    fn verify_rejects_high_s_malleable_twin() {
        use std::ops::Neg;
        let key = Es256SigningKey::generate();
        let jwks = Jwks::from_public_key(&key.public_key());
        let signed = ReceiptSigner::sign(sample_body(), &key).unwrap();

        let jws = signed.jws_compact();
        let mut parts = jws.split('.');
        let header_b64 = parts.next().unwrap();
        let payload_b64 = parts.next().unwrap();
        let sig_b64 = parts.next().unwrap();
        let sig = Signature::from_slice(&B64URL.decode(sig_b64).unwrap()).unwrap();
        let low = sig.normalize_s().unwrap_or(sig);
        let high = Signature::from_scalars(low.r(), low.s().neg()).expect("n - s_low is nonzero");
        let malicious = format!(
            "{header_b64}.{payload_b64}.{}",
            B64URL.encode(high.to_bytes())
        );
        let forged = SignedReceipt::from_parts(malicious, signed.body().clone());

        match ReceiptVerifier::verify(&forged, &jwks) {
            Err(ReceiptError::SignatureInvalid) => {}
            other => panic!("expected SignatureInvalid for high-S twin, got {other:?}"),
        }
    }
}
