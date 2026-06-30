use crate::error::WebhookError;
use hmac::{Hmac, KeyInit, Mac};
use secrecy::{ExposeSecret, SecretString};
use sha2::Sha256;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Verify an HMAC-SHA256 signature over a raw body.
///
/// `signature_hex` is the expected hex-encoded HMAC digest.
/// Returns `Ok(())` if the signature matches, otherwise [`WebhookError::SignatureVerificationFailed`].
pub fn verify_signature(
    body: &[u8],
    secret: &SecretString,
    signature_hex: &str,
) -> Result<(), WebhookError> {
    let mut mac = HmacSha256::new_from_slice(secret.expose_secret().as_bytes())
        .map_err(|e| WebhookError::Internal(format!("HMAC init failed: {e}")))?;
    mac.update(body);
    let expected =
        hex::decode(signature_hex).map_err(|_| WebhookError::SignatureVerificationFailed)?;
    let computed = mac.finalize().into_bytes();
    if expected.len() != computed.len() {
        return Err(WebhookError::SignatureVerificationFailed);
    }
    if expected.ct_eq(&computed).into() {
        Ok(())
    } else {
        Err(WebhookError::SignatureVerificationFailed)
    }
}

/// Generate an HMAC-SHA256 signature for a body (hex-encoded).
pub fn sign_body(body: &[u8], secret: &SecretString) -> Result<String, WebhookError> {
    let mut mac = HmacSha256::new_from_slice(secret.expose_secret().as_bytes())
        .map_err(|e| WebhookError::Internal(format!("HMAC init failed: {e}")))?;
    mac.update(body);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::SecretString;

    #[test]
    fn test_sign_and_verify() {
        let secret = SecretString::new("test-secret".into());
        let body = b"test body";
        let sig = sign_body(body, &secret).unwrap();
        assert_eq!(sig.len(), 64);
        assert!(verify_signature(body, &secret, &sig).is_ok());
    }

    #[test]
    fn test_verify_bad_secret() {
        let secret = SecretString::new("test-secret".into());
        let body = b"test body";
        let sig = sign_body(body, &secret).unwrap();
        let bad = SecretString::new("wrong".into());
        assert!(verify_signature(body, &bad, &sig).is_err());
    }

    #[test]
    fn test_tampered_body() {
        let secret = SecretString::new("test-secret".into());
        let body = b"test body";
        let signature = sign_body(body, &secret).unwrap();
        let tampered = b"tampered body";
        let result = verify_signature(tampered, &secret, &signature);
        assert!(matches!(
            result,
            Err(WebhookError::SignatureVerificationFailed)
        ));
    }
}
