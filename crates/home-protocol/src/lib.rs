//! Ardur device wire protocol. This crate never executes bots or opens connections.
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::pkcs8::{
    DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey, LineEnding,
};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairingPayload {
    pub version: u8,
    pub challenge: String,
    pub instance_id: String,
    pub home_name: String,
    pub fingerprint: String,
    pub certificate_fingerprint: String,
    pub hints: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HomePins {
    pub instance_id: String,
    pub fingerprint: String,
    pub certificate_fingerprint: String,
}
impl PairingPayload {
    pub fn pins(&self) -> HomePins {
        HomePins {
            instance_id: self.instance_id.clone(),
            fingerprint: self.fingerprint.clone(),
            certificate_fingerprint: self.certificate_fingerprint.clone(),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub instance_id: String,
    pub fingerprint: String,
    pub certificate: String,
    pub signature: String,
    pub nonce: Option<String>,
    pub timestamp: Option<i64>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Proof {
    pub grant_id: String,
    pub nonce: String,
    pub timestamp: i64,
    pub signature: String,
}
/// Errors deliberately contain no caller input or key material.
#[derive(Debug)]
pub struct InvalidProtocol;
impl std::fmt::Display for InvalidProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Invalid device protocol data.")
    }
}
impl std::error::Error for InvalidProtocol {}

pub fn string_len(value: &str) -> usize {
    value.encode_utf16().count()
}
pub fn fingerprint_valid(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn https_url(value: &str) -> Result<url::Url, InvalidProtocol> {
    let u = url::Url::parse(value).map_err(|_| InvalidProtocol)?;
    if u.scheme() != "https"
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err(InvalidProtocol);
    }
    Ok(u)
}
pub fn decode_pairing_code(code: &str) -> Result<PairingPayload, InvalidProtocol> {
    if string_len(code) > 16384 {
        return Err(InvalidProtocol);
    }
    let bytes = if code.trim_start().starts_with('{') {
        code.as_bytes().to_vec()
    } else {
        // Node accepts padded URL base64 as well as the unpadded exported code.
        URL_SAFE_NO_PAD
            .decode(code.trim().trim_end_matches('='))
            .map_err(|_| InvalidProtocol)?
    };
    let p: PairingPayload = serde_json::from_slice(&bytes).map_err(|_| InvalidProtocol)?;
    if p.version != 1
        || !(32..=128).contains(&string_len(&p.challenge))
        || !(1..=128).contains(&string_len(&p.instance_id))
        || !(1..=80).contains(&string_len(&p.home_name))
        || !fingerprint_valid(&p.fingerprint)
        || !fingerprint_valid(&p.certificate_fingerprint)
        || p.hints.len() > 8
        || p.hints.iter().any(|h| https_url(h).is_err())
    {
        return Err(InvalidProtocol);
    }
    Ok(p)
}
/// Mirrors JS number formatting and UTF-16 key ordering, not RFC 8785 or Rust ordering.
pub fn canonical_json(value: &Value) -> String {
    match value {
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(canonical_json).collect::<Vec<_>>().join(",")
        ),
        Value::Object(o) => {
            let mut entries = o.iter().collect::<Vec<_>>();
            entries.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(k, v)| format!(
                        "{}:{}",
                        serde_json::to_string(k).expect("string JSON"),
                        canonical_json(v)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Value::Number(n) => {
            let f = n.as_f64().expect("JSON finite number");
            if f == 0.0 {
                "0".into()
            } else {
                ryu_js::Buffer::new().format(f).to_owned()
            }
        }
        _ => serde_json::to_string(value).expect("JSON value"),
    }
}
pub fn pairing_signed_text(
    p: &PairingPayload,
    public_key: &str,
    presence_public_key: &str,
) -> String {
    canonical_json(&json!([
        "ardur-pair-v1",
        p.instance_id,
        p.challenge,
        public_key,
        presence_public_key
    ]))
}
pub fn device_signed_text(instance: &str, p: &Proof, operation: &str, body: &Value) -> String {
    canonical_json(&json!([
        "ardur-device-v1",
        instance,
        p.grant_id,
        p.nonce,
        p.timestamp,
        operation,
        body
    ]))
}
pub fn home_signed_text(instance: &str, fingerprint: &str, challenge: &str) -> String {
    canonical_json(&json!(["ardur-home-v1", instance, fingerprint, challenge]))
}
pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn nonce() -> String {
    let mut b = [0u8; 32];
    OsRng.fill_bytes(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}
pub struct DeviceKeys {
    pub public_key: String,
    pub presence_public_key: String,
    pub private_key: Zeroizing<String>,
}
impl DeviceKeys {
    pub fn generate() -> Result<Self, InvalidProtocol> {
        let request = SigningKey::random(&mut OsRng);
        let presence = SigningKey::random(&mut OsRng);
        Ok(Self {
            public_key: STANDARD.encode(
                request
                    .verifying_key()
                    .to_public_key_der()
                    .map_err(|_| InvalidProtocol)?
                    .as_bytes(),
            ),
            presence_public_key: STANDARD.encode(
                presence
                    .verifying_key()
                    .to_public_key_der()
                    .map_err(|_| InvalidProtocol)?
                    .as_bytes(),
            ),
            private_key: request
                .to_pkcs8_pem(LineEnding::LF)
                .map_err(|_| InvalidProtocol)?,
        })
    }
    pub fn sign(&self, text: &str) -> Result<String, InvalidProtocol> {
        sign_text(&self.private_key, text)
    }
}
pub fn sign_text(private_key: &str, text: &str) -> Result<String, InvalidProtocol> {
    let key = SigningKey::from_pkcs8_pem(private_key).map_err(|_| InvalidProtocol)?;
    let sig: Signature = key.sign(text.as_bytes());
    Ok(STANDARD.encode(sig.to_der().as_bytes()))
}
pub fn verify_device_signature(public_key: &str, text: &str, signature: &str) -> bool {
    let check = || -> Option<()> {
        if public_key.len() > 256 || signature.len() > 256 {
            return None;
        }
        let key = VerifyingKey::from_public_key_der(&STANDARD.decode(public_key).ok()?).ok()?;
        let sig = Signature::from_der(&STANDARD.decode(signature).ok()?).ok()?;
        key.verify(text.as_bytes(), &sig).ok()
    };
    check().is_some()
}
pub fn certificate_matches(raw: &[u8], fingerprint: &str, now: i64) -> bool {
    let Ok((rest, cert)) = x509_parser::parse_x509_certificate(raw) else {
        return false;
    };
    rest.is_empty()
        && sha256(raw) == fingerprint
        && cert.validity().not_before.timestamp().saturating_mul(1000) <= now
        && now < cert.validity().not_after.timestamp().saturating_mul(1000)
}
pub fn verify_home(p: &HomePins, challenge: &str, i: &Identity, now: i64) -> bool {
    let check = || -> Option<()> {
        if i.instance_id != p.instance_id || i.fingerprint != p.fingerprint {
            return None;
        }
        let raw = STANDARD.decode(&i.certificate).ok()?;
        if !certificate_matches(&raw, &p.certificate_fingerprint, now) {
            return None;
        }
        let (_, cert) = x509_parser::parse_x509_certificate(&raw).ok()?;
        let spki = cert.public_key();
        if sha256(spki.raw) != p.fingerprint {
            return None;
        }
        let text = home_signed_text(&p.instance_id, &p.fingerprint, challenge);
        let sig = STANDARD.decode(&i.signature).ok()?;
        match spki.algorithm.algorithm.to_id_string().as_str() {
            "1.2.840.113549.1.1.1" => ring::signature::UnparsedPublicKey::new(
                &ring::signature::RSA_PKCS1_2048_8192_SHA256,
                spki.subject_public_key.data.as_ref(),
            )
            .verify(text.as_bytes(), &sig)
            .ok(),
            "1.2.840.10045.2.1" => {
                let key = VerifyingKey::from_public_key_der(spki.raw).ok()?;
                key.verify(text.as_bytes(), &Signature::from_der(&sig).ok()?)
                    .ok()
            }
            _ => None,
        }
    };
    check().is_some()
}
pub fn valid_nonce(i: &Identity, now: i64) -> bool {
    matches!((&i.nonce,i.timestamp),(Some(n),Some(t)) if (32..=128).contains(&string_len(n)) && t>0 && t<=9_007_199_254_740_991 && now.abs_diff(t)<=60_000)
}

pub fn certificate_key_fingerprint(raw: &[u8]) -> Option<String> {
    let (rest, cert) = x509_parser::parse_x509_certificate(raw).ok()?;
    if !rest.is_empty() {
        return None;
    }
    Some(sha256(cert.public_key().raw))
}
