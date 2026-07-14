//! A minimal, hand-rolled AWS Signature Version 4 signer for one signed POST.
//!
//! Scoped to exactly what [`crate::BedrockProvider`] needs — a single JSON
//! body POST to Bedrock Runtime's `InvokeModel` — rather than a general
//! SigV4 library. See <https://docs.aws.amazon.com/general/latest/gr/sigv4-create-canonical-request.html>
//! for the algorithm this implements.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// The headers + query string a signed request must send, computed by
/// [`sign`].
pub struct SignedRequest {
    /// The `Authorization` header value.
    pub authorization: String,
    /// The `x-amz-date` header value (also folded into `authorization`'s
    /// credential scope).
    pub amz_date: String,
}

/// AWS credentials used to sign one request. `session_token` is `Some` for
/// STS-issued temporary credentials (adds `x-amz-security-token` to the
/// signed headers).
pub struct Credentials<'a> {
    /// AWS access key id.
    pub access_key_id: &'a str,
    /// AWS secret access key.
    pub secret_access_key: &'a str,
    /// Optional STS session token.
    pub session_token: Option<&'a str>,
}

/// Sign a `POST {host}{canonical_uri}` request carrying `body` (already
/// serialized) for the `bedrock` service in `region`, at `now`.
///
/// `canonical_uri` must already be percent-encoded (see [`uri_encode`]) and
/// begin with `/`. The caller sends `authorization`, `x-amz-date`, and (when
/// `credentials.session_token` is `Some`) `x-amz-security-token` as request
/// headers alongside `content-type: application/json` and `host`.
pub fn sign(
    credentials: &Credentials<'_>,
    region: &str,
    host: &str,
    canonical_uri: &str,
    body: &[u8],
    now: chrono::DateTime<chrono::Utc>,
) -> SignedRequest {
    const SERVICE: &str = "bedrock";
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date_stamp = now.format("%Y%m%d").to_string();

    let payload_hash = hex::encode(Sha256::digest(body));

    // Canonical headers must be sorted by lowercase header name.
    let mut header_pairs: Vec<(&str, String)> = vec![
        ("content-type", "application/json".to_string()),
        ("host", host.to_string()),
        ("x-amz-date", amz_date.clone()),
    ];
    if let Some(token) = credentials.session_token {
        header_pairs.push(("x-amz-security-token", token.to_string()));
    }
    header_pairs.sort_by(|a, b| a.0.cmp(b.0));

    let canonical_headers: String = header_pairs
        .iter()
        .map(|(k, v)| format!("{k}:{v}\n"))
        .collect();
    let signed_headers = header_pairs
        .iter()
        .map(|(k, _)| *k)
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request =
        format!("POST\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
    let canonical_request_hash = hex::encode(Sha256::digest(canonical_request.as_bytes()));

    let credential_scope = format!("{date_stamp}/{region}/{SERVICE}/aws4_request");
    let string_to_sign =
        format!("AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{canonical_request_hash}");

    let signing_key =
        derive_signing_key(credentials.secret_access_key, &date_stamp, region, SERVICE);
    let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));

    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key_id
    );

    SignedRequest {
        authorization,
        amz_date,
    }
}

fn derive_signing_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts a key of any length");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

/// Percent-encode `segment` per AWS's URI-encoding rules for a single path
/// segment: unreserved characters (`A-Za-z0-9-_.~`) pass through unescaped,
/// everything else is percent-encoded uppercase-hex. Bedrock model ids such
/// as `anthropic.claude-3-5-sonnet-20241022-v2:0` need this for the `:`.
#[must_use]
pub fn uri_encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_encode_escapes_colon_and_preserves_unreserved() {
        assert_eq!(
            uri_encode("anthropic.claude-3-5-sonnet-20241022-v2:0"),
            "anthropic.claude-3-5-sonnet-20241022-v2%3A0"
        );
    }

    #[test]
    fn sign_produces_a_well_formed_authorization_header() {
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            session_token: None,
        };
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-14T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let signed = sign(
            &creds,
            "us-east-1",
            "bedrock-runtime.us-east-1.amazonaws.com",
            "/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/invoke",
            b"{}",
            now,
        );
        assert!(signed.authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20260714/us-east-1/bedrock/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature="));
        assert_eq!(signed.amz_date, "20260714T000000Z");
    }

    #[test]
    fn session_token_is_folded_into_signed_headers() {
        let creds = Credentials {
            access_key_id: "AKIDEXAMPLE",
            secret_access_key: "secret",
            session_token: Some("token123"),
        };
        let now = chrono::Utc::now();
        let signed = sign(
            &creds,
            "us-east-1",
            "bedrock-runtime.us-east-1.amazonaws.com",
            "/model/foo/invoke",
            b"{}",
            now,
        );
        assert!(
            signed
                .authorization
                .contains("SignedHeaders=content-type;host;x-amz-date;x-amz-security-token")
        );
    }
}
