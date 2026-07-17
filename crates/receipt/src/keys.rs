//! ES256 key material and the JWKS publication format.
//!
//! Keys are thin wrappers over `p256` so the rest of the crate never touches
//! the curve crate directly. Signing is deterministic (RFC 6979); only key
//! generation draws from the OS CSPRNG.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use p256::EncodedPoint;
use p256::ecdsa::{SigningKey, VerifyingKey};
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};

use crate::error::ReceiptError;
use crate::types::Sha256Digest;

/// An ES256 (NIST P-256) private signing key.
#[derive(Clone)]
pub struct Es256SigningKey(SigningKey);

impl Es256SigningKey {
    /// Generate a fresh key from the operating-system CSPRNG.
    pub fn generate() -> Self {
        Self(SigningKey::random(&mut OsRng))
    }

    /// Parse a PKCS#8 PEM-encoded private key.
    pub fn from_pkcs8_pem(pem: &str) -> Result<Self, ReceiptError> {
        SigningKey::from_pkcs8_pem(pem)
            .map(Self)
            .map_err(|e| ReceiptError::Malformed(format!("pkcs8 decode: {e}")))
    }

    /// Serialize to a PKCS#8 PEM string (LF line endings).
    pub fn to_pkcs8_pem(&self) -> Result<String, ReceiptError> {
        self.0
            .to_pkcs8_pem(LineEnding::LF)
            .map(|pem| pem.to_string())
            .map_err(|e| ReceiptError::Malformed(format!("pkcs8 encode: {e}")))
    }

    /// The matching public key.
    pub fn public_key(&self) -> Es256PublicKey {
        Es256PublicKey(*self.0.verifying_key())
    }

    /// The JWS `kid` for this key — first 16 hex chars of
    /// `SHA256(public_key_bytes)`.
    pub fn key_id(&self) -> String {
        self.public_key().key_id()
    }

    pub(crate) fn inner(&self) -> &SigningKey {
        &self.0
    }
}

/// An ES256 (NIST P-256) public verification key.
#[derive(Clone, Copy)]
pub struct Es256PublicKey(VerifyingKey);

impl Es256PublicKey {
    /// The JWS `kid` for this key — first 16 hex chars of
    /// `SHA256(public_key_bytes)`, where the bytes are the SEC1 uncompressed
    /// encoding.
    pub fn key_id(&self) -> String {
        let point = self.0.to_encoded_point(false);
        Sha256Digest::of(point.as_bytes()).to_hex()[..16].to_string()
    }

    /// Render as a JWK, with `kid` set to this key's [`Es256PublicKey::key_id`].
    pub fn to_jwk(&self) -> JwksKey {
        let point = self.0.to_encoded_point(false);
        JwksKey {
            kid: self.key_id(),
            kty: "EC".to_string(),
            crv: "P-256".to_string(),
            x: B64URL.encode(point.x().expect("uncompressed point has x")),
            y: B64URL.encode(point.y().expect("uncompressed point has y")),
        }
    }

    /// Reconstruct a public key from its JWK affine coordinates.
    pub fn from_jwk(jwk: &JwksKey) -> Result<Self, ReceiptError> {
        if jwk.kty != "EC" || jwk.crv != "P-256" {
            return Err(ReceiptError::Malformed(format!(
                "unsupported JWK kty/crv: {}/{}",
                jwk.kty, jwk.crv
            )));
        }
        let x = decode_coord(&jwk.x)?;
        let y = decode_coord(&jwk.y)?;
        let point = EncodedPoint::from_affine_coordinates(
            p256::FieldBytes::from_slice(&x),
            p256::FieldBytes::from_slice(&y),
            false,
        );
        let key = VerifyingKey::from_encoded_point(&point)
            .map_err(|e| ReceiptError::Malformed(format!("invalid JWK point: {e}")))?;
        Ok(Self(key))
    }

    pub(crate) fn inner(&self) -> &VerifyingKey {
        &self.0
    }
}

fn decode_coord(s: &str) -> Result<[u8; 32], ReceiptError> {
    let bytes = B64URL
        .decode(s)
        .map_err(|e| ReceiptError::Malformed(format!("base64url coord: {e}")))?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| ReceiptError::Malformed("JWK coordinate is not 32 bytes".to_string()))?;
    Ok(arr)
}

/// One key in a [`Jwks`] — a P-256 EC public key in JWK form (RFC 7517/7518).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JwksKey {
    /// Key id; matches the `kid` carried in the JWS protected header.
    pub kid: String,
    /// Key type — always `EC` for this crate.
    pub kty: String,
    /// Curve — always `P-256`.
    pub crv: String,
    /// Base64url (no pad) of the 32-byte affine x-coordinate.
    pub x: String,
    /// Base64url (no pad) of the 32-byte affine y-coordinate.
    pub y: String,
}

/// A set of published verification keys, keyed by `kid`. Serializes as a bare
/// JSON array of [`JwksKey`].
//
// TODO §11.14 Phase 2: JWKS rotation — overlapping `kid` windows, an `exp`
// per key, and fetching the Authority-published set over HTTP.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Jwks(pub Vec<JwksKey>);

impl Jwks {
    /// An empty key set.
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// A key set holding the single public key `pk`.
    pub fn from_public_key(pk: &Es256PublicKey) -> Self {
        Self(vec![pk.to_jwk()])
    }

    /// Append a key.
    pub fn push(&mut self, key: JwksKey) {
        self.0.push(key);
    }

    /// Find the key with the given `kid`.
    pub fn lookup(&self, kid: &str) -> Option<&JwksKey> {
        self.0.iter().find(|k| k.kid == kid)
    }
}
