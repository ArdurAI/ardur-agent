//! [`VapidKeyPair`] — the server's VAPID (RFC 8292) identity key, generated
//! once and persisted to disk so it survives a restart.
//!
//! Mirrors the P-256/PKCS8 custody pattern `ardur-receipt`'s
//! `Es256SigningKey` already uses in this workspace, rather than introducing
//! a second ECC convention.

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use p256::ecdsa::SigningKey;
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rand_core::OsRng;

use crate::error::PwaError;

/// A VAPID identity key: a P-256 keypair used to sign the `Authorization:
/// vapid` JWT on every push send, so the push service can verify the sender.
pub struct VapidKeyPair {
    signing_key: SigningKey,
}

impl VapidKeyPair {
    /// Generate a fresh key from the operating-system CSPRNG.
    #[must_use]
    pub fn generate() -> Self {
        Self {
            signing_key: SigningKey::random(&mut OsRng),
        }
    }

    /// Load the key from `path` if it exists, otherwise generate a fresh one
    /// and persist it there (parent directories are created as needed).
    ///
    /// # Errors
    /// [`PwaError::KeyLoad`] if the existing file fails to parse;
    /// [`PwaError::Persist`] if a freshly generated key cannot be written.
    pub async fn load_or_generate(path: &Path) -> Result<Self, PwaError> {
        match tokio::fs::read_to_string(path).await {
            Ok(pem) => SigningKey::from_pkcs8_pem(&pem)
                .map(|signing_key| Self { signing_key })
                .map_err(|e| PwaError::KeyLoad(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = Self::generate();
                key.persist(path).await?;
                Ok(key)
            }
            Err(e) => Err(PwaError::KeyLoad(e.to_string())),
        }
    }

    /// Write the PKCS#8 PEM encoding of this key to `path`.
    ///
    /// # Errors
    /// [`PwaError::Persist`] if the parent directory or file cannot be
    /// created/written.
    pub async fn persist(&self, path: &Path) -> Result<(), PwaError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| PwaError::Persist(e.to_string()))?;
        }
        let pem = self
            .signing_key
            .to_pkcs8_pem(LineEnding::LF)
            .map_err(|e| PwaError::Persist(e.to_string()))?;
        tokio::fs::write(path, pem.as_bytes())
            .await
            .map_err(|e| PwaError::Persist(e.to_string()))
    }

    /// The PKCS#8 PEM encoding — the form [`web_push::VapidSignatureBuilder`]
    /// expects.
    ///
    /// # Errors
    /// [`PwaError::KeyLoad`] if the key cannot be re-encoded (not expected in
    /// practice; the key was either parsed from or generated as valid PKCS8).
    pub(crate) fn pkcs8_pem(&self) -> Result<String, PwaError> {
        self.signing_key
            .to_pkcs8_pem(LineEnding::LF)
            .map(|pem| pem.to_string())
            .map_err(|e| PwaError::KeyLoad(e.to_string()))
    }

    /// The uncompressed public key point, base64url-encoded (no padding) —
    /// the form a browser's `PushManager.subscribe({ applicationServerKey })`
    /// expects.
    #[must_use]
    pub fn public_key_b64url(&self) -> String {
        let point = self.signing_key.verifying_key().to_encoded_point(false);
        B64URL.encode(point.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn load_or_generate_persists_and_reloads_the_same_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("vapid.pem");

        let first = VapidKeyPair::load_or_generate(&path)
            .await
            .expect("generates on first load");
        assert!(path.exists(), "the key is persisted to disk");

        let second = VapidKeyPair::load_or_generate(&path)
            .await
            .expect("reloads on second load");
        assert_eq!(
            first.public_key_b64url(),
            second.public_key_b64url(),
            "the same key is reloaded rather than regenerated"
        );
    }

    #[test]
    fn public_key_b64url_is_url_safe_no_padding() {
        let key = VapidKeyPair::generate();
        let encoded = key.public_key_b64url();
        assert!(
            !encoded.contains('+') && !encoded.contains('/') && !encoded.contains('='),
            "expected URL-safe, unpadded base64, got: {encoded}"
        );
    }
}
