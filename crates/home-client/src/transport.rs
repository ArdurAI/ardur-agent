use crate::{Error, encode_body};
use home_protocol::{certificate_matches, fingerprint_valid, https_url};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Debug)]
struct PinVerifier {
    fingerprint: String,
    failed: Arc<AtomicBool>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}
impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let ms = now.as_secs().saturating_mul(1000).min(i64::MAX as u64) as i64;
        if !certificate_matches(end.as_ref(), &self.fingerprint, ms) {
            self.failed.store(true, Ordering::SeqCst);
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ));
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
pub struct PinnedTransport {
    client: reqwest::Client,
    failed: Arc<AtomicBool>,
}
impl PinnedTransport {
    pub fn new(fingerprint: &str) -> Result<Self, Error> {
        if !fingerprint_valid(fingerprint) {
            return Err(Error::Input);
        }
        let failed = Arc::new(AtomicBool::new(false));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|_| Error::Protocol)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinVerifier {
                fingerprint: fingerprint.into(),
                failed: failed.clone(),
                provider,
            }))
            .with_no_client_auth();
        // No application bytes in TLS 0-RTT, before the pin or handshake proof.
        tls.enable_early_data = false;
        let client = reqwest::Client::builder()
            .use_preconfigured_tls(tls)
            .no_proxy()
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(15))
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|_| Error::Protocol)?;
        Ok(Self { client, failed })
    }
    pub async fn post(&self, url: &str, body: &Value) -> Result<Value, Error> {
        let target = https_url(url).map_err(|_| Error::Input)?;
        let mut response = self
            .client
            .post(target)
            .header("content-type", "application/json")
            .body(encode_body(body))
            .send()
            .await
            .map_err(|e| {
                // Certificate failure is access refusal; never include reqwest's URL/error text.
                if self.failed.load(Ordering::SeqCst) {
                    return Error::Identity;
                }
                use std::error::Error as _;
                let mut source = e.source();
                while let Some(s) = source {
                    if s.downcast_ref::<rustls::Error>().is_some() {
                        return Error::Identity;
                    }
                    source = s.source();
                }
                Error::Unreachable
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(if status.as_u16() == 401 || status.as_u16() == 403 {
                Error::Access
            } else {
                Error::Protocol
            });
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Unreachable)? {
            if bytes.len().saturating_add(chunk.len()) > 2 * 1024 * 1024 {
                return Err(Error::Protocol);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)
    }
}
