#![allow(dead_code)]
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use home_client::now_ms;
use home_protocol::{PairingPayload, Proof, home_signed_text, sha256, sign_text};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
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
    LostAdmission,
    HungRead,
    RunningThenCancelled,
    LostRoomAdmission,
    BoardDenied,
    BoardProblem,
    RoomSendRefused,
    RecordUnavailable,
}
const DEVICE_RECORD_UNAVAILABLE: &str = "This record is unavailable from this device.";
pub struct EventResponse {
    pub chunks: Vec<Vec<u8>>,
    pub interrupted: bool,
    pub finish_run: bool,
}
pub struct FakeHome {
    pub event_windows: Arc<Mutex<VecDeque<EventResponse>>>,
    pub payload: PairingPayload,
    pub certificate: rustls::pki_types::CertificateDer<'static>,
    pub calls: Arc<AtomicUsize>,
    pub decoded_bytes: Arc<AtomicUsize>,
    pub mode: Arc<Mutex<Mode>>,
    pub seen: Arc<Mutex<Vec<Value>>>,
    pub run: Arc<Mutex<Value>>,
    pub messages: Arc<Mutex<Value>>,
    pub admissions: Arc<Mutex<HashMap<String, Value>>>,
    pub rooms: Arc<Mutex<Value>>,
    pub computers: Arc<Mutex<Value>>,
    pub board: Arc<Mutex<Value>>,
    pub board_item: Arc<Mutex<Value>>,
    pub room_result: Arc<Mutex<Value>>,
    pub room_admissions: Arc<Mutex<HashMap<String, Value>>>,
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
        let decoded_bytes = Arc::new(AtomicUsize::new(0));
        let byte_count = decoded_bytes.clone();
        let mode = Arc::new(Mutex::new(Mode::Normal));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let run = Arc::new(Mutex::new(
            json!({"taskId":"task","runId":"run","threadId":"thread","botId":"bot","state":"done","cancelRequested":false,"status":"completed","cancelConfirmed":false,"messageId":"answer","failure":null,"createdAt":"2026-01-01T00:00:00Z","startedAt":null,"completedAt":null}),
        ));
        let messages = Arc::new(Mutex::new(
            json!({"threadId":"thread","messages":[{"id":"answer","runId":"run","role":"bot","blocks":[{"kind":"text","text":"Fixture answer ✓"},{"kind":"text","text":"sensitive-canary","reasoning":true},{"kind":"tool-result","text":"sensitive-canary"}]}],"olderCursor":null}),
        ));
        let admissions = Arc::new(Mutex::new(HashMap::<String, Value>::new()));
        // Stage 4 fixture-shaped defaults: the copied home vectors double as
        // the fake home's room, computer and board state.
        let rooms = Arc::new(Mutex::new(
            json!([{"id":"fixture-room","spaceId":"fixture-space","name":"Fixture room","pinned":false,"sectionId":null,"archivedAt":null,"threadId":"fixture-thread","preview":"","unread":false,"members":[{"botId":"fixture-chief","name":"Chief","color":"ink"},{"botId":"fixture-worker","name":"Beta","color":"ink"}],"updatedAt":"2026-10-01T00:00:00.000Z","createdAt":"2026-10-01T00:00:00.000Z"}]),
        ));
        let computers = Arc::new(Mutex::new(
            json!([{"botId":"fixture-bot","name":"Fixture bot","status":{"computerId":"fixture-shared-computer","botId":"fixture-bot","mode":"team","kind":"docker","state":"stopped","controlHolder":"none","controlBotId":null,"takeoverRequested":false,"screenAvailable":false,"screenWidth":1280,"screenHeight":720,"homeRevision":null,"busyBotName":null,"canUpdate":false}}]),
        ));
        let board = Arc::new(Mutex::new(
            json!({"items":[{"id":"work-1","title":"Fixture task","description":"","acceptanceCriteria":"","type":"task","status":"open","priority":2,"assignee":null,"labels":[],"parent":null,"dependencies":[],"dueAt":null,"deferUntil":null,"estimateMinutes":null,"externalRef":null,"createdAt":"2026-10-01T00:00:00.000Z","updatedAt":"2026-10-01T00:00:00.000Z","closedAt":null,"commentCount":0,"comments":[],"history":[],"closeWhenDone":false}],"readyIds":["work-1"],"blockedIds":[]}),
        ));
        let board_item = Arc::new(Mutex::new(
            json!({"id":"work-1","title":"Fixture task","description":"","acceptanceCriteria":"","type":"task","status":"open","priority":2,"assignee":null,"labels":[],"parent":null,"dependencies":[],"dueAt":null,"deferUntil":null,"estimateMinutes":null,"externalRef":null,"createdAt":"2026-10-01T00:00:00.000Z","updatedAt":"2026-10-01T00:00:00.000Z","closedAt":null,"commentCount":0,"comments":[],"history":[],"closeWhenDone":false}),
        ));
        let room_result = Arc::new(Mutex::new(
            json!({"kind":"work","taskId":"fixture-task","runId":"fixture-run-1","runIds":["fixture-run-1","fixture-run-2"],"seq":1}),
        ));
        let room_admissions = Arc::new(Mutex::new(HashMap::<String, Value>::new()));
        let (run_state, message_state, admitted) =
            (run.clone(), messages.clone(), admissions.clone());
        let (
            room_state,
            computer_state,
            board_state,
            board_item_state,
            room_send_state,
            room_admitted,
        ) = (
            rooms.clone(),
            computers.clone(),
            board.clone(),
            board_item.clone(),
            room_result.clone(),
            room_admissions.clone(),
        );
        let event_windows = Arc::new(Mutex::new(VecDeque::<EventResponse>::new()));
        let windows = event_windows.clone();
        let certificate = der.clone();
        let (p, c, m, s) = (payload.clone(), calls.clone(), mode.clone(), seen.clone());
        let task = tokio::spawn(async move {
            let acceptor = TlsAcceptor::from(Arc::new(config));
            let mut device_key = String::new();
            let mut paired = false;
            let mut used = HashSet::new();
            let mut issued = HashSet::new();
            let mut nonce_counter = 0;
            let mut run_reads = 0;
            let mut lost = false;
            let mut room_lost = false;
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
                    byte_count.fetch_add(1, Ordering::SeqCst);
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
                let mut received = 0;
                while received < len {
                    match tls.read(&mut body[received..]).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            byte_count.fetch_add(n, Ordering::SeqCst);
                            received += n;
                        }
                    }
                }
                if received != len {
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
                    let text = independent_pairing_text(
                        &p,
                        value["devicePublicKey"].as_str().unwrap(),
                        value["presencePublicKey"].as_str().unwrap(),
                    );
                    let valid = value["challenge"] == p.challenge
                        && value["instanceId"] == p.instance_id
                        && value["platform"] == "cli"
                        && value["devicePublicKey"] != value["presencePublicKey"]
                        && independent_verify(
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
                    let text = independent_request_text(&p.instance_id, &proof, op, &value["body"]);
                    if proof.grant_id != "fake-grant"
                        || now_ms().abs_diff(proof.timestamp) > 60000
                        || !issued.contains(&proof.nonce)
                        || used.contains(&proof.nonce)
                        || !independent_verify(&device_key, &text, &proof.signature)
                    {
                        status = 401;
                        json!({"message":"unavailable"})
                    } else {
                        used.insert(proof.nonce);
                        if op == "events" {
                            let response = windows
                                .lock()
                                .unwrap()
                                .pop_front()
                                .expect("configured event window");
                            let bytes: usize = response.chunks.iter().map(Vec::len).sum();
                            let headers = format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                                bytes + usize::from(response.interrupted)
                            );
                            let _ = tls.write_all(headers.as_bytes()).await;
                            for chunk in response.chunks {
                                let _ = tls.write_all(&chunk).await;
                                let _ = tls.flush().await;
                                tokio::task::yield_now().await;
                            }
                            if response.finish_run {
                                run_state.lock().unwrap()["status"] = json!("completed");
                            }
                            let _ = tls.shutdown().await;
                            continue;
                        } else if op == "tasks" {
                            json!([])
                        } else if op == "rpc"
                            && value["body"] == json!({"procedure":"bots/list","input":{}})
                        {
                            json!([{"id":"bot","name":"Bot\u{1b}]0;bad\u{7}","threadId":"thread","status":"idle","modelProvider":null,"modelId":null,"thinkingLevel":null,"runtimeKind":"pi","instructions":"sensitive-canary","runtimeConfig":{"secret":"sensitive-canary"}}])
                        } else if op == "rpc"
                            && value["body"]["procedure"].as_str() == Some("computer/list")
                        {
                            if mode == Mode::RecordUnavailable {
                                status = 403;
                                json!({"message":DEVICE_RECORD_UNAVAILABLE})
                            } else {
                                computer_state.lock().unwrap().clone()
                            }
                        } else if op == "rpc"
                            && value["body"]["procedure"].as_str() == Some("board/snapshot")
                        {
                            if mode == Mode::RecordUnavailable {
                                status = 403;
                                json!({"message":DEVICE_RECORD_UNAVAILABLE})
                            } else if mode == Mode::BoardDenied {
                                status = 403;
                                board_denied()
                            } else if mode == Mode::BoardProblem {
                                status = 400;
                                board_problem()
                            } else {
                                board_state.lock().unwrap().clone()
                            }
                        } else if op == "rpc"
                            && value["body"]["procedure"].as_str() == Some("board/show")
                        {
                            if mode == Mode::RecordUnavailable {
                                status = 403;
                                json!({"message":DEVICE_RECORD_UNAVAILABLE})
                            } else if mode == Mode::BoardDenied {
                                status = 403;
                                board_denied()
                            } else if mode == Mode::BoardProblem {
                                status = 400;
                                board_problem()
                            } else {
                                board_item_state.lock().unwrap().clone()
                            }
                        } else if op == "rooms/list" {
                            if mode == Mode::RecordUnavailable {
                                status = 403;
                                json!({"message":DEVICE_RECORD_UNAVAILABLE})
                            } else {
                                room_state.lock().unwrap().clone()
                            }
                        } else if op == "rooms/send" {
                            let body = &value["body"];
                            if mode == Mode::RoomSendRefused {
                                status = 403;
                                json!({"message":"This action is unavailable from this device."})
                            } else {
                                let by_name = body.get("roomName").and_then(Value::as_str);
                                let rooms = room_state.lock().unwrap();
                                let group_id = if let Some(name) = by_name {
                                    let found: Vec<String> = rooms
                                        .as_array()
                                        .unwrap()
                                        .iter()
                                        .filter(|r| {
                                            r["archivedAt"].is_null()
                                                && r["name"]
                                                    .as_str()
                                                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
                                        })
                                        .filter_map(|r| r["id"].as_str().map(str::to_owned))
                                        .collect();
                                    if found.len() == 1 {
                                        Some(found[0].clone())
                                    } else {
                                        None
                                    }
                                } else {
                                    body.get("groupId")
                                        .and_then(Value::as_str)
                                        .map(str::to_owned)
                                };
                                let room_thread = group_id.and_then(|id| {
                                    rooms
                                        .as_array()
                                        .unwrap()
                                        .iter()
                                        .find(|r| r["id"] == id && r["archivedAt"].is_null())
                                        .map(|r| r["threadId"].clone())
                                });
                                drop(rooms);
                                match room_thread {
                                    None => {
                                        status = if by_name.is_some() { 400 } else { 403 };
                                        json!({"message":DEVICE_RECORD_UNAVAILABLE})
                                    }
                                    Some(room_thread)
                                        if body.get("threadId").is_some()
                                            && body["threadId"] != room_thread =>
                                    {
                                        status = 403;
                                        json!({"message":DEVICE_RECORD_UNAVAILABLE})
                                    }
                                    Some(_) => {
                                        let nonce = body["clientNonce"].as_str().unwrap();
                                        let mut admissions = room_admitted.lock().unwrap();
                                        if let Some(old) = admissions.get(nonce) {
                                            if old["request"] != *body {
                                                status = 409;
                                                json!({"message":"This request changed; send it as a new task."})
                                            } else {
                                                old["result"].clone()
                                            }
                                        } else {
                                            // Admission is durable before the
                                            // response; a lost response replays it.
                                            let result = room_send_state.lock().unwrap().clone();
                                            admissions.insert(
                                                nonce.to_owned(),
                                                json!({"request":body.clone(),"result":result.clone()}),
                                            );
                                            if mode == Mode::LostRoomAdmission && !room_lost {
                                                room_lost = true;
                                                continue;
                                            }
                                            result
                                        }
                                    }
                                }
                            }
                        } else if op == "dispatch" {
                            let body = &value["body"];
                            let id = body["clientNonce"].as_str().unwrap();
                            let changed = {
                                let mut entries = admitted.lock().unwrap();
                                if let Some(old) = entries.get(id) {
                                    old != body
                                } else {
                                    entries.insert(id.into(), body.clone());
                                    false
                                }
                            };
                            if changed {
                                status = 409;
                                json!({"message":"sensitive-canary changed input"})
                            } else {
                                if mode == Mode::LostAdmission && !lost {
                                    lost = true;
                                    continue;
                                }
                                json!({"taskId":"task","runId":"run","threadId":"thread","botId":"bot","state":"accepted","cancelRequested":false})
                            }
                        } else if op == "runs/get" || op == "tasks/get" {
                            if mode == Mode::HungRead {
                                std::future::pending::<()>().await;
                            }
                            let mut run = run_state.lock().unwrap();
                            if mode == Mode::RunningThenCancelled {
                                run_reads += 1;
                                run["status"] = json!(if run_reads == 1 {
                                    "running"
                                } else {
                                    "cancelled"
                                });
                                run["state"] =
                                    json!(if run_reads == 1 { "running" } else { "stopped" });
                                run["cancelRequested"] = json!(true);
                                run["cancelConfirmed"] = json!(run_reads > 1);
                            }
                            let field = if op == "runs/get" { "run" } else { "task" };
                            json!({field:run.clone()})
                        } else if op == "runs/list" {
                            json!({"runs":[run_state.lock().unwrap().clone()],"nextCursor":"run"})
                        } else if op == "messages/get" {
                            if admitted.lock().unwrap().is_empty() {
                                status = 403;
                                json!({"message":"unavailable"})
                            } else {
                                message_state.lock().unwrap().clone()
                            }
                        } else if op == "stop" {
                            run_state.lock().unwrap()["cancelRequested"] = json!(true);
                            json!({"cancelRequested":true})
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
            event_windows,
            payload,
            certificate,
            calls,
            decoded_bytes,
            mode,
            seen,
            run,
            messages,
            admissions,
            rooms,
            computers,
            board,
            board_item,
            room_result,
            room_admissions,
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

// Board refusals carry the home's shared fixed problem answers.
fn board_denied() -> Value {
    json!({"message":"This board is only available to this computer's owner.","problem":{"code":"access_lost","message":"This board is only available to this computer's owner."}})
}
fn board_problem() -> Value {
    json!({"message":"This folder has no board","problem":{"code":"no_board","message":"This folder has no board"}})
}

// Independent fixture home: construct the wire contract directly and use ring's
// DER ECDSA verifier rather than the protocol crate's text/signature functions.
fn independent_pairing_text(p: &PairingPayload, key: &str, presence: &str) -> String {
    serde_json::to_string(&json!([
        "ardur-pair-v1",
        p.instance_id,
        p.challenge,
        key,
        presence
    ]))
    .unwrap()
}
fn independent_request_text(
    instance: &str,
    proof: &Proof,
    operation: &str,
    body: &Value,
) -> String {
    // Stage 2 routes have fixed bodies; later stages sort JSON independently.
    // These routes accept only empty input or bots/list. Their canonical bytes
    // are fixed by the committed TypeScript vectors, without a Rust canonicalizer.
    let body = if operation == "tasks" && body == &json!({}) {
        "{}".to_owned()
    } else if operation == "rpc" && body == &json!({"procedure":"bots/list","input":{}}) {
        r#"{"input":{},"procedure":"bots/list"}"#.to_owned()
    } else if operation == "rpc" && body == &json!({"procedure":"computer/list","input":null}) {
        r#"{"input":null,"procedure":"computer/list"}"#.to_owned()
    } else if [
        "dispatch",
        "events",
        "runs/get",
        "tasks/get",
        "runs/list",
        "messages/get",
        "stop",
        "rooms/list",
        "rooms/send",
    ]
    .contains(&operation)
        || (operation == "rpc"
            && body
                .get("procedure")
                .and_then(Value::as_str)
                .is_some_and(|p| p.starts_with("board/")))
    {
        independent_canonical(body)
    } else {
        return String::new();
    };
    let prefix = serde_json::to_string(&json!([
        "ardur-device-v1",
        instance,
        proof.grant_id,
        proof.nonce,
        proof.timestamp,
        operation
    ]))
    .unwrap();
    format!("{},{}]", &prefix[..prefix.len() - 1], body)
}
fn independent_verify(public_key: &str, text: &str, signature: &str) -> bool {
    let (Ok(spki), Ok(signature)) = (STANDARD.decode(public_key), STANDARD.decode(signature))
    else {
        return false;
    };
    // Fixed P-256 SubjectPublicKeyInfo prefix (ecPublicKey / prime256v1).
    const PREFIX: &[u8] = &[
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
        0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
    ];
    let Some(key) = spki
        .strip_prefix(PREFIX)
        .filter(|key| key.len() == 65 && key[0] == 4)
    else {
        return false;
    };
    !text.is_empty()
        && ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, key)
            .verify(text.as_bytes(), &signature)
            .is_ok()
}
#[test]
fn independent_home_verifier_matches_typescript_vectors() {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../home-protocol/tests/fixtures/typescript.json"
    ))
    .unwrap();
    let p: PairingPayload = serde_json::from_value(fixture["payload"].clone()).unwrap();
    let key = fixture["publicKey"].as_str().unwrap();
    let pairing = independent_pairing_text(&p, key, fixture["presencePublicKey"].as_str().unwrap());
    assert_eq!(pairing, fixture["pairingText"]);
    assert!(independent_verify(
        key,
        &pairing,
        fixture["pairingSignature"].as_str().unwrap()
    ));
    let proof: Proof = serde_json::from_value(json!({
        "grantId":fixture["proof"]["grantId"], "nonce":fixture["proof"]["nonce"],
        "timestamp":fixture["proof"]["timestamp"], "signature":""
    }))
    .unwrap();
    let request = fixture["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["body"] == json!({"procedure":"bots/list","input":{}}))
        .unwrap();
    let text = independent_request_text(&p.instance_id, &proof, "rpc", &request["body"]);
    assert_eq!(text, request["text"]);
    assert!(independent_verify(
        key,
        &text,
        request["signature"].as_str().unwrap()
    ));
    assert!(!independent_verify(
        key,
        &(text + "changed"),
        request["signature"].as_str().unwrap()
    ));
}

fn independent_canonical(value: &Value) -> String {
    match value {
        Value::Object(o) => {
            let mut keys = o.keys().collect::<Vec<_>>();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|k| format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap(),
                        independent_canonical(&o[k])
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter()
                .map(independent_canonical)
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => value.to_string(),
    }
}
#[test]
fn independent_home_verifier_matches_every_stage3_typescript_vector() {
    let f: Value = serde_json::from_str(include_str!(
        "../../../home-protocol/tests/fixtures/typescript-stage3.json"
    ))
    .unwrap();
    let mut proof = f["proof"].clone();
    proof["signature"] = json!("");
    let proof: Proof = serde_json::from_value(proof).unwrap();
    for r in f["requests"].as_array().unwrap() {
        let text = independent_request_text(
            f["instanceId"].as_str().unwrap(),
            &proof,
            r["operation"].as_str().unwrap(),
            &r["body"],
        );
        assert_eq!(text, r["signedText"]);
        assert!(independent_verify(
            f["publicKey"].as_str().unwrap(),
            &text,
            r["signature"].as_str().unwrap()
        ));
    }
}
