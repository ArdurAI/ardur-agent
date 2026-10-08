#![allow(dead_code)]
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use home_client::now_ms;
use home_protocol::{
    PairingPayload, Proof, device_signed_text, home_signed_text, pairing_signed_text, sha256,
    sign_text, verify_device_signature,
};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Normal,
    WrongInstance,
    WrongFingerprint,
    BadSignature,
    StaleNonce,
    BadNonce,
    Replay,
    Revoked,
    Redirect,
    Malformed,
    Oversize,
    SecretError,
}
pub struct FakeHome {
    pub payload: PairingPayload,
    pub calls: Arc<AtomicUsize>,
    pub mode: Arc<Mutex<Mode>>,
    pub seen: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for FakeHome {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl FakeHome {
    pub async fn start(expired: bool) -> Self {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
        if expired {
            params.not_before = rcgen::date_time_ymd(2000, 1, 1);
            params.not_after = rcgen::date_time_ymd(2001, 1, 1);
        }
        let cert = params.self_signed(&key).unwrap();
        let der = cert.der().clone();
        let pem = key.serialize_pem();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![der.clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let payload = PairingPayload {
            version: 1,
            challenge: "synthetic-challenge".repeat(3),
            instance_id: "fake-home".into(),
            home_name: "Home 雪\u{1b}[2J".into(),
            fingerprint: home_protocol::certificate_key_fingerprint(der.as_ref()).unwrap(),
            certificate_fingerprint: sha256(der.as_ref()),
            hints: vec![format!(
                "https://127.0.0.1:{}",
                listener.local_addr().unwrap().port()
            )],
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let mode = Arc::new(Mutex::new(Mode::Normal));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (p, c, m, s) = (payload.clone(), calls.clone(), mode.clone(), seen.clone());
        let task = tokio::spawn(async move {
            let acceptor = TlsAcceptor::from(Arc::new(config));
            let mut device_key = String::new();
            let mut paired = false;
            let mut used = HashSet::new();
            let mut issued = HashSet::new();
            let mut nonce_counter = 0;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    continue;
                };
                let mut headers = Vec::new();
                let mut one = [0];
                while !headers.ends_with(b"\r\n\r\n") && headers.len() < 8192 {
                    if tls.read_exact(&mut one).await.is_err() {
                        break;
                    }
                    headers.push(one[0]);
                }
                if !headers.ends_with(b"\r\n\r\n") {
                    continue;
                }
                c.fetch_add(1, Ordering::SeqCst);
                let headers = String::from_utf8(headers).unwrap();
                let len = headers
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if len > 65536 {
                    continue;
                }
                let mut body = vec![0; len];
                if tls.read_exact(&mut body).await.is_err() {
                    continue;
                }
                let value: Value = serde_json::from_slice(&body).unwrap();
                s.lock().unwrap().push(value.clone());
                let path = headers
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                let mode = *m.lock().unwrap();
                let mut status = 200;
                let response = if mode == Mode::Redirect {
                    status = 302;
                    json!({"message":"sensitive-canary"})
                } else if mode == Mode::SecretError {
                    status = 500;
                    json!({"message":format!("sensitive-canary {pem}")})
                } else if path == "/device/nonce" {
                    let challenge = value["clientChallenge"].as_str().unwrap();
                    let mut result = json!({"instanceId":p.instance_id,"fingerprint":p.fingerprint,"certificate":STANDARD.encode(der.as_ref()),"signature":sign_text(&pem,&home_signed_text(&p.instance_id,&p.fingerprint,challenge)).unwrap()});
                    if mode == Mode::WrongInstance {
                        result["instanceId"] = json!("wrong")
                    }
                    if mode == Mode::WrongFingerprint {
                        result["fingerprint"] = json!("0".repeat(64))
                    }
                    if mode == Mode::BadSignature {
                        result["signature"] = json!(STANDARD.encode(b"wrong"))
                    }
                    if value.get("grantId").is_some() {
                        if !paired || value["grantId"] != "fake-grant" || mode == Mode::Revoked {
                            status = 401;
                            result = json!({"message":"sensitive-canary"})
                        } else {
                            nonce_counter += 1;
                            let nonce = if mode == Mode::Replay {
                                "r".repeat(43)
                            } else {
                                format!("{nonce_counter:043}")
                            };
                            issued.insert(nonce.clone());
                            result["nonce"] = json!(if mode == Mode::BadNonce {
                                "short".into()
                            } else {
                                nonce
                            });
                            result["timestamp"] =
                                json!(now_ms() - if mode == Mode::StaleNonce { 61000 } else { 0 });
                        }
                    }
                    result
                } else if path == "/device/pair" {
                    let text = pairing_signed_text(
                        &p,
                        value["devicePublicKey"].as_str().unwrap(),
                        value["presencePublicKey"].as_str().unwrap(),
                    );
                    let valid = value["challenge"] == p.challenge
                        && value["instanceId"] == p.instance_id
                        && value["platform"] == "cli"
                        && value["devicePublicKey"] != value["presencePublicKey"]
                        && verify_device_signature(
                            value["devicePublicKey"].as_str().unwrap(),
                            &text,
                            value["signature"].as_str().unwrap(),
                        );
                    if !valid || paired {
                        status = 401;
                        json!({"message":"unavailable"})
                    } else {
                        paired = true;
                        device_key = value["devicePublicKey"].as_str().unwrap().into();
                        json!({"grantId":"fake-grant","spaceId":"fake-space","instanceId":p.instance_id})
                    }
                } else if path == "/device/request" {
                    let proof: Proof = serde_json::from_value(value["proof"].clone()).unwrap();
                    let op = value["operation"].as_str().unwrap();
                    let text = device_signed_text(&p.instance_id, &proof, op, &value["body"]);
                    if proof.grant_id != "fake-grant"
                        || now_ms().abs_diff(proof.timestamp) > 60000
                        || !issued.contains(&proof.nonce)
                        || used.contains(&proof.nonce)
                        || !verify_device_signature(&device_key, &text, &proof.signature)
                    {
                        status = 401;
                        json!({"message":"unavailable"})
                    } else {
                        used.insert(proof.nonce);
                        if op == "tasks" {
                            json!([])
                        } else if op == "rpc"
                            && value["body"] == json!({"procedure":"bots/list","input":{}})
                        {
                            json!([{"id":"bot","name":"Bot\u{1b}]0;bad\u{7}","threadId":"thread","status":"idle","modelProvider":null,"modelId":null,"thinkingLevel":null,"runtimeKind":"pi","instructions":"sensitive-canary","runtimeConfig":{"secret":"sensitive-canary"}}])
                        } else {
                            status = 403;
                            json!({"message":"unavailable"})
                        }
                    }
                } else {
                    status = 404;
                    json!({})
                };
                let encoded = if mode == Mode::Malformed {
                    "sensitive-canary not json".into()
                } else if mode == Mode::Oversize {
                    "x".repeat(2 * 1024 * 1024 + 1)
                } else {
                    response.to_string()
                };
                let wire = format!(
                    "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nlocation: https://example.invalid\r\nconnection: close\r\n\r\n{encoded}",
                    encoded.len()
                );
                let _ = tls.write_all(wire.as_bytes()).await;
                let _ = tls.shutdown().await;
            }
        });
        Self {
            payload,
            calls,
            mode,
            seen,
            task,
        }
    }
    pub fn code(&self) -> String {
        serde_json::to_string(&self.payload).unwrap()
    }
    pub fn set(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode
    }
    pub fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}
