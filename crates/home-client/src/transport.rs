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
    #[cfg(test)]
    tls: Arc<rustls::ClientConfig>,
}
impl PinnedTransport {
    pub fn new(fingerprint: &str) -> Result<Self, Error> {
        let (tls, failed) = Self::tls_config(fingerprint)?;
        Self::with_tls(tls, failed)
    }
    fn tls_config(fingerprint: &str) -> Result<(rustls::ClientConfig, Arc<AtomicBool>), Error> {
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
        // Resumed sessions skip certificate verification, including expiry.
        tls.resumption = rustls::client::Resumption::disabled();
        Ok((tls, failed))
    }
    fn with_tls(tls: rustls::ClientConfig, failed: Arc<AtomicBool>) -> Result<Self, Error> {
        #[cfg(test)]
        let test_tls = Arc::new(tls.clone());
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
        Ok(Self {
            client,
            failed,
            #[cfg(test)]
            tls: test_tls,
        })
    }
    pub async fn post(&self, url: &str, body: &Value) -> Result<Value, Error> {
        let (status, bytes) = self.post_raw(url, body).await?;
        if !(200..300).contains(&status) {
            return Err(Self::status_error(status));
        }
        serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)
    }
    /// Like [`post`], but a refused request (HTTP 400/403) with the home's own
    /// bounded printable answer keeps that answer instead of the generic map.
    pub async fn post_answer(&self, url: &str, body: &Value) -> Result<Value, Refusal> {
        let (status, bytes) = self.post_raw(url, body).await.map_err(Refusal::Failure)?;
        if (200..300).contains(&status) {
            return serde_json::from_slice(&bytes).map_err(|_| Refusal::Failure(Error::Protocol));
        }
        if status == 400 || status == 403 {
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                if let Some(answer) = fixed_answer(&value) {
                    return Err(Refusal::Answer {
                        status,
                        code: value["problem"]["code"].as_str().map(str::to_owned),
                        message: answer,
                    });
                }
            }
        }
        Err(Refusal::Failure(Self::status_error(status)))
    }
    async fn post_raw(&self, url: &str, body: &Value) -> Result<(u16, Vec<u8>), Error> {
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
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Unreachable)? {
            if bytes.len().saturating_add(chunk.len()) > 2 * 1024 * 1024 {
                return Err(Error::Protocol);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok((status, bytes))
    }
    fn status_error(status: u16) -> Error {
        if status == 401 || status == 403 {
            Error::Access
        } else if status == 409 {
            Error::RequestChanged
        } else {
            Error::Protocol
        }
    }
}
/// A refused device request whose home answer carries a fixed printable sentence.
#[derive(Debug)]
pub enum Refusal {
    /// HTTP 400/403 with the home's own bounded printable answer.
    Answer {
        status: u16,
        code: Option<String>,
        message: String,
    },
    /// Any other failure, mapped to the generic errors.
    Failure(Error),
}
impl Refusal {
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Answer { status, .. } => Some(*status),
            Self::Failure(_) => None,
        }
    }
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Answer { code, .. } => code.as_deref(),
            Self::Failure(_) => None,
        }
    }
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Answer { message, .. } => Some(message),
            Self::Failure(_) => None,
        }
    }
}
/// The home's fixed answer sentence, accepted only when it is bounded and free
/// of control characters; anything else keeps the generic refusal.
fn fixed_answer(value: &Value) -> Option<String> {
    let message = value.get("message").and_then(Value::as_str)?;
    if (1..=200).contains(&home_protocol::string_len(message))
        && !message.chars().any(char::is_control)
    {
        Some(message.to_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::PrivatePkcs8KeyDer;
    use std::io::{Cursor, Read, Write};
    use std::sync::atomic::AtomicU64;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Debug)]
    struct Clock(AtomicU64);
    impl rustls::time_provider::TimeProvider for Clock {
        fn current_time(&self) -> Option<UnixTime> {
            Some(UnixTime::since_unix_epoch(Duration::from_secs(
                self.0.load(Ordering::SeqCst),
            )))
        }
    }
    fn setup(
        version: &'static rustls::SupportedProtocolVersion,
    ) -> (
        rustls::ClientConfig,
        Arc<AtomicBool>,
        Arc<rustls::ServerConfig>,
        Arc<Clock>,
    ) {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
        params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        params.not_after = rcgen::date_time_ymd(2030, 1, 1);
        let cert = params.self_signed(&key).unwrap();
        // 2030-01-01 00:00:00 UTC, matching not_after above.
        let clock = Arc::new(Clock(AtomicU64::new(1_893_456_000 - 1)));
        let (mut tls, failed) =
            PinnedTransport::tls_config(&home_protocol::sha256(cert.der().as_ref())).unwrap();
        tls.time_provider = clock.clone();
        // The server allows exactly one version, so negotiation is unambiguous.
        let mut server = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[version])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        )
        .unwrap();
        server.time_provider = clock.clone();
        server.ticketer = rustls::crypto::ring::Ticketer::new().unwrap();
        server.send_tls13_tickets = 2;
        (tls, failed, Arc::new(server), clock)
    }
    fn exchange(
        client: &mut rustls::ClientConnection,
        server: &mut rustls::ServerConnection,
    ) -> Result<(), rustls::Error> {
        for _ in 0..64 {
            let mut progress = false;
            if client.wants_write() {
                let mut wire = Vec::new();
                client.write_tls(&mut wire).unwrap();
                server.read_tls(&mut Cursor::new(wire)).unwrap();
                server.process_new_packets()?;
                progress = true;
            }
            if server.wants_write() {
                let mut wire = Vec::new();
                server.write_tls(&mut wire).unwrap();
                client.read_tls(&mut Cursor::new(wire)).unwrap();
                client.process_new_packets()?;
                progress = true;
            }
            if !progress {
                return Ok(());
            }
        }
        panic!("TLS exchange did not settle");
    }
    fn memory_request(
        tls: Arc<rustls::ClientConfig>,
        config: Arc<rustls::ServerConfig>,
    ) -> (usize, Option<rustls::HandshakeKind>) {
        let mut client =
            rustls::ClientConnection::new(tls, ServerName::try_from("localhost").unwrap()).unwrap();
        let mut server = rustls::ServerConnection::new(config).unwrap();
        if exchange(&mut client, &mut server).is_ok() {
            assert!(!client.is_handshaking());
            client
                .writer()
                .write_all(b"POST /device/nonce HTTP/1.1\r\ncontent-length: 2\r\n\r\n{}")
                .unwrap();
            exchange(&mut client, &mut server).unwrap();
        }
        let mut decoded = [0; 1024];
        let bytes = server.reader().read(&mut decoded).unwrap_or(0);
        (bytes, server.handshake_kind())
    }
    #[test]
    fn reused_transport_rechecks_expiry_in_memory_tls12_and_tls13() {
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            let (mut tls, _failed, server, clock) = setup(version);
            // Positive control: this server actually resumes and the default
            // configuration reproduces HTTP delivery after certificate expiry.
            tls.resumption = rustls::client::Resumption::default();
            let vulnerable = Arc::new(tls.clone());
            assert!(memory_request(vulnerable.clone(), server.clone()).0 > 0);
            clock.0.fetch_add(2, Ordering::SeqCst);
            let (bytes, kind) = memory_request(vulnerable, server.clone());
            assert_eq!(kind, Some(rustls::HandshakeKind::Resumed));
            assert!(bytes > 0);
            let (tls, failed, server, clock) = setup(version);
            let transport = PinnedTransport::with_tls(tls, failed).unwrap();
            assert!(memory_request(transport.tls.clone(), server.clone()).0 > 0);
            clock.0.fetch_add(2, Ordering::SeqCst);
            assert_eq!(memory_request(transport.tls.clone(), server).0, 0);
            assert!(transport.failed.load(Ordering::SeqCst));
        }
    }
    #[tokio::test]
    async fn reused_transport_posts_no_bytes_after_expiry_tls12_and_tls13() {
        for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
            let (tls, failed, config, clock) = setup(version);
            let transport = PinnedTransport::with_tls(tls, failed).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("https://{}/device/nonce", listener.local_addr().unwrap());
            let bytes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = bytes.clone();
            let task = tokio::spawn(async move {
                let acceptor = tokio_rustls::TlsAcceptor::from(config);
                for _ in 0..2 {
                    let (socket, _) = listener.accept().await.unwrap();
                    if let Ok(mut stream) = acceptor.accept(socket).await {
                        let mut request = Vec::new();
                        let mut byte = [0];
                        while stream.read_exact(&mut byte).await.is_ok() {
                            counted.fetch_add(1, Ordering::SeqCst);
                            request.push(byte[0]);
                            if request.ends_with(b"\r\n\r\n") {
                                break;
                            }
                        }
                        let mut body = [0; 2];
                        let mut read = 0;
                        while read < body.len() {
                            match stream.read(&mut body[read..]).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    counted.fetch_add(n, Ordering::SeqCst);
                                    read += n;
                                }
                            }
                        }
                        stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}").await.unwrap();
                        let _ = stream.shutdown().await;
                    }
                }
            });
            assert_eq!(
                transport.post(&url, &serde_json::json!({})).await.unwrap(),
                serde_json::json!({})
            );
            let before = bytes.load(Ordering::SeqCst);
            assert!(before > 0);
            clock.0.fetch_add(2, Ordering::SeqCst);
            assert!(matches!(
                transport.post(&url, &serde_json::json!({})).await,
                Err(Error::Identity)
            ));
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(bytes.load(Ordering::SeqCst) - before, 0);
        }
    }
}
