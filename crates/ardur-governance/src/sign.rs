//! ES256 JWS Compact signing + verification of Execution Receipts, and the
//! hash-chained mirror log.
//!
//! Wire format matches the MCEP ER spec §9: a JWS Compact
//! (`base64url(header).base64url(payload).base64url(sig)`) with protected header
//! `alg=ES256`, `typ=application/ardur.er+jwt`, and the signer's `kid`. The
//! signature is IEEE P1363 fixed-width R‖S with canonical low-S (ARD-483 parity
//! with `ardur-receipt`).
//!
//! Keys are raw `p256` but the published key set is `ardur_receipt::Jwks`, and
//! the `kid` derivation is identical to the receipt crate — so a governed
//! runtime signs native receipts and Execution Receipts with the same P-256 key
//! and publishes a single JWKS for both.

use ardur_receipt::{Jwks, JwksKey};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use p256::ecdsa::signature::{Signer as _, Verifier as _};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use p256::{EncodedPoint, FieldBytes};
use serde::{Deserialize, Serialize};

use crate::er::ExecutionReceipt;
use crate::error::GovernanceError;
use crate::hash::{sha256_hex, to_hex};

/// JWS `alg` for ES256 Execution Receipts.
const ALG: &str = "ES256";
/// JWS `typ` for an MCEP Execution Receipt (spec §9.1).
pub const ER_TYP: &str = "application/ardur.er+jwt";

#[derive(Serialize, Deserialize)]
struct JwsHeader {
    alg: String,
    kid: String,
    typ: String,
}

/// An ES256 (NIST P-256) signing key for Execution Receipts.
#[derive(Clone)]
pub struct ErSigningKey {
    inner: SigningKey,
    kid: String,
}

impl ErSigningKey {
    /// Generate a fresh key from the OS CSPRNG.
    pub fn generate() -> Self {
        let inner = SigningKey::random(&mut rand_core::OsRng);
        let kid = kid_of(inner.verifying_key());
        Self { inner, kid }
    }

    /// Load a key from PKCS#8 PEM — the same custody format `ardur-receipt` uses,
    /// so a governed runtime can sign native receipts and ERs with one key.
    pub fn from_pkcs8_pem(pem: &str) -> Result<Self, GovernanceError> {
        let inner = SigningKey::from_pkcs8_pem(pem)
            .map_err(|e| GovernanceError::Key(format!("pkcs8 decode: {e}")))?;
        let kid = kid_of(inner.verifying_key());
        Ok(Self { inner, kid })
    }

    /// Serialize to PKCS#8 PEM (LF line endings).
    pub fn to_pkcs8_pem(&self) -> Result<String, GovernanceError> {
        self.inner
            .to_pkcs8_pem(LineEnding::LF)
            .map(|pem| pem.to_string())
            .map_err(|e| GovernanceError::Key(format!("pkcs8 encode: {e}")))
    }

    /// The JWS `kid` (first 16 hex of `SHA256(SEC1-uncompressed pubkey)` —
    /// identical to `ardur-receipt`).
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// The public verification key set for this key, in `ardur-receipt` JWKS
    /// form. A governed runtime merges this with its native-receipt JWKS (they
    /// share a `kid` when the same key signs both).
    pub fn jwks(&self) -> Jwks {
        Jwks(vec![jwk_of(self.verifying_key(), &self.kid)])
    }

    fn verifying_key(&self) -> &VerifyingKey {
        self.inner.verifying_key()
    }
}

/// An Execution Receipt wrapped in a verifiable ES256 JWS. The compact JWS is
/// the canonical form a child's `parent_receipt_hash` is computed over.
#[derive(Clone, Debug, PartialEq)]
pub struct SignedExecutionReceipt {
    jws_compact: String,
    receipt: ExecutionReceipt,
}

impl SignedExecutionReceipt {
    /// The compact JWS string (`header.payload.sig`).
    pub fn jws_compact(&self) -> &str {
        &self.jws_compact
    }

    /// The decoded claim set.
    pub fn receipt(&self) -> &ExecutionReceipt {
        &self.receipt
    }

    /// Hex SHA-256 of the compact JWS — the `parent_receipt_hash` a child
    /// receipt in the same lineage must carry.
    pub fn receipt_hash(&self) -> String {
        sha256_hex(self.jws_compact.as_bytes())
    }
}

/// Signs [`ExecutionReceipt`] claim sets into [`SignedExecutionReceipt`]s.
pub struct ErSigner;

impl ErSigner {
    /// Sign `receipt` with `key`.
    pub fn sign(
        receipt: ExecutionReceipt,
        key: &ErSigningKey,
    ) -> Result<SignedExecutionReceipt, GovernanceError> {
        let header = JwsHeader {
            alg: ALG.to_string(),
            kid: key.kid.clone(),
            typ: ER_TYP.to_string(),
        };
        let header_json =
            serde_json::to_vec(&header).map_err(|e| GovernanceError::Sign(e.to_string()))?;
        let payload_json =
            serde_json::to_vec(&receipt).map_err(|e| GovernanceError::Sign(e.to_string()))?;
        let signing_input = format!(
            "{}.{}",
            B64URL.encode(header_json),
            B64URL.encode(payload_json)
        );

        let sig: Signature = key
            .inner
            .try_sign(signing_input.as_bytes())
            .map_err(|e| GovernanceError::Sign(format!("sign: {e}")))?;
        // ARD-483: emit canonical low-S so a high-S-rejecting verifier accepts.
        let sig = sig.normalize_s().unwrap_or(sig);
        let jws_compact = format!("{signing_input}.{}", B64URL.encode(sig.to_bytes()));

        Ok(SignedExecutionReceipt {
            jws_compact,
            receipt,
        })
    }
}

/// Verifies Execution Receipt JWSs against an `ardur-receipt` [`Jwks`].
pub struct ErVerifier;

impl ErVerifier {
    /// Verify a compact ER JWS: resolve `kid`, check `alg`/`typ`, verify the
    /// ES256 signature (rejecting high-S), and decode the claim set.
    pub fn verify_compact(
        jws_compact: &str,
        jwks: &Jwks,
    ) -> Result<ExecutionReceipt, GovernanceError> {
        let mut parts = jws_compact.split('.');
        let (header_b64, payload_b64, sig_b64) =
            match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some(h), Some(p), Some(s), None) => (h, p, s),
                _ => {
                    return Err(GovernanceError::Verify(
                        "expected three JWS Compact segments".to_string(),
                    ));
                }
            };

        let header_bytes = B64URL
            .decode(header_b64)
            .map_err(|e| GovernanceError::Verify(format!("header base64url: {e}")))?;
        let header: JwsHeader = serde_json::from_slice(&header_bytes)
            .map_err(|e| GovernanceError::Verify(format!("header json: {e}")))?;
        if header.alg != ALG {
            return Err(GovernanceError::Verify(format!(
                "unsupported alg `{}`",
                header.alg
            )));
        }
        if header.typ != ER_TYP {
            return Err(GovernanceError::Verify(format!(
                "unsupported typ `{}`",
                header.typ
            )));
        }

        let jwk = jwks
            .lookup(&header.kid)
            .ok_or_else(|| GovernanceError::Verify(format!("unknown kid `{}`", header.kid)))?;
        let pubkey = verifying_key_from_jwk(jwk)?;

        let sig_bytes = B64URL
            .decode(sig_b64)
            .map_err(|_| GovernanceError::Verify("signature base64url".to_string()))?;
        let sig = Signature::from_slice(&sig_bytes)
            .map_err(|_| GovernanceError::Verify("signature bytes".to_string()))?;
        // ARD-483: reject the malleable high-S twin.
        if sig.normalize_s().is_some() {
            return Err(GovernanceError::Verify("non-canonical high-S".to_string()));
        }
        let signing_input = format!("{header_b64}.{payload_b64}");
        pubkey
            .verify(signing_input.as_bytes(), &sig)
            .map_err(|_| GovernanceError::Verify("signature does not verify".to_string()))?;

        let payload_bytes = B64URL
            .decode(payload_b64)
            .map_err(|e| GovernanceError::Verify(format!("payload base64url: {e}")))?;
        serde_json::from_slice(&payload_bytes)
            .map_err(|e| GovernanceError::Verify(format!("payload json: {e}")))
    }
}

/// Verify a hash-chained mirror log of Execution Receipts: signature first, then
/// linkage (`parent_receipt_hash[i] == SHA256(jws[i-1])`, and
/// `parent_receipt_id[i] == parent_receipt_hash[i][..16]`; genesis carries both
/// as `null`).
pub fn verify_er_chain(
    receipts: &[SignedExecutionReceipt],
    jwks: &Jwks,
) -> Result<(), GovernanceError> {
    let mut prev_hash: Option<String> = None;
    for (i, signed) in receipts.iter().enumerate() {
        let claims = ErVerifier::verify_compact(signed.jws_compact(), jwks).map_err(|e| {
            GovernanceError::BrokenChain {
                index: i,
                detail: format!("signature: {e}"),
            }
        })?;
        if claims.parent_receipt_hash != prev_hash {
            return Err(GovernanceError::BrokenChain {
                index: i,
                detail: format!(
                    "parent_receipt_hash {:?} != expected {:?}",
                    claims.parent_receipt_hash, prev_hash
                ),
            });
        }
        // parent_receipt_id mirrors the reference impl: hash[..16], or null.
        let want_pid = prev_hash.as_ref().map(|h| h[..16].to_string());
        if claims.parent_receipt_id != want_pid {
            return Err(GovernanceError::BrokenChain {
                index: i,
                detail: format!(
                    "parent_receipt_id {:?} != expected {:?}",
                    claims.parent_receipt_id, want_pid
                ),
            });
        }
        prev_hash = Some(signed.receipt_hash());
    }
    Ok(())
}

/// The JWS `kid` for a verifying key — first 16 hex of `SHA256(SEC1
/// uncompressed pubkey)`. Identical to `ardur_receipt::Es256PublicKey::key_id`.
fn kid_of(vk: &VerifyingKey) -> String {
    let point = vk.to_encoded_point(false);
    to_hex(&crate::hash::sha256(point.as_bytes()))[..16].to_string()
}

/// Render a verifying key as an `ardur-receipt` JWK.
fn jwk_of(vk: &VerifyingKey, kid: &str) -> JwksKey {
    let point = vk.to_encoded_point(false);
    JwksKey {
        kid: kid.to_string(),
        kty: "EC".to_string(),
        crv: "P-256".to_string(),
        x: B64URL.encode(point.x().expect("uncompressed point has x")),
        y: B64URL.encode(point.y().expect("uncompressed point has y")),
    }
}

/// Reconstruct a verifying key from a JWK's affine coordinates.
fn verifying_key_from_jwk(jwk: &JwksKey) -> Result<VerifyingKey, GovernanceError> {
    if jwk.kty != "EC" || jwk.crv != "P-256" {
        return Err(GovernanceError::Verify(format!(
            "unsupported JWK kty/crv: {}/{}",
            jwk.kty, jwk.crv
        )));
    }
    let x = decode_coord(&jwk.x)?;
    let y = decode_coord(&jwk.y)?;
    let point = EncodedPoint::from_affine_coordinates(
        FieldBytes::from_slice(&x),
        FieldBytes::from_slice(&y),
        false,
    );
    VerifyingKey::from_encoded_point(&point)
        .map_err(|e| GovernanceError::Verify(format!("invalid JWK point: {e}")))
}

fn decode_coord(s: &str) -> Result<[u8; 32], GovernanceError> {
    let bytes = B64URL
        .decode(s)
        .map_err(|e| GovernanceError::Verify(format!("base64url coord: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| GovernanceError::Verify("JWK coordinate is not 32 bytes".to_string()))
}
