use home_protocol::*;
use serde::Deserialize;
use serde_json::Value;
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    instance_id: String,
    proof: Value,
    requests: Vec<Request>,
    responses: Vec<Response>,
    rejected: Vec<Rejected>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Request {
    operation: String,
    body: Value,
    canonical_body: String,
    signed_text: String,
}
#[derive(Deserialize)]
struct Response {
    operation: String,
    #[serde(default)]
    procedure: Option<String>,
    body: Value,
}
#[derive(Deserialize)]
struct Rejected {
    body: Box<serde_json::value::RawValue>,
}
fn fixture() -> Fixture {
    serde_json::from_str(include_str!("fixtures/device-operations.json")).unwrap()
}
#[test]
fn copied_home_vectors_preserve_every_signed_byte_and_refuse_surrogates() {
    let f = fixture();
    let mut proof = f.proof;
    proof["signature"] = Value::String(String::new());
    let proof: Proof = serde_json::from_value(proof).unwrap();
    assert_eq!(f.requests.len(), 14);
    for r in f.requests {
        assert_eq!(canonical_json(&r.body), r.canonical_body);
        assert_eq!(
            device_signed_text(&f.instance_id, &proof, &r.operation, &r.body),
            r.signed_text
        );
    }
    assert_eq!(f.rejected.len(), 4);
    for r in f.rejected {
        assert!(matches!(
            parse_json(r.body.get().as_bytes()),
            Err(InvalidProtocol::InvalidUnicode)
        ));
    }
}
#[test]
fn copied_home_responses_parse_into_typed_stage4_records() {
    let f = fixture();
    assert_eq!(f.responses.len(), 7);
    let mut work = None;
    let mut greeting = None;
    let mut rooms = None;
    let mut computers = None;
    let mut snapshot = None;
    let mut shown = None;
    let mut denied = None;
    for r in &f.responses {
        match (r.operation.as_str(), r.procedure.as_deref()) {
            ("rooms/send", _) if r.body["kind"] == "work" => work = Some(r),
            ("rooms/send", _) => greeting = Some(r),
            ("rooms/list", _) => rooms = Some(r),
            ("rpc", Some("computer/list")) => computers = Some(r),
            ("rpc", Some("board/snapshot")) => snapshot = Some(r),
            ("rpc", Some("board/show")) if r.body["problem"].is_null() => shown = Some(r),
            ("rpc", Some("board/show")) => denied = Some(r),
            other => panic!("unexpected response {other:?}"),
        }
    }
    // A work answer keeps every run id; a greeting invents no task or run.
    let parsed: ThreadSendResult = serde_json::from_value(work.unwrap().body.clone()).unwrap();
    let ThreadSendResult::Work {
        run_id, run_ids, ..
    } = parsed
    else {
        panic!("work response parsed as receipt-only")
    };
    let run_ids = run_ids.expect("work response keeps the full runIds array");
    assert_eq!(
        run_ids,
        vec!["fixture-run-1".to_owned(), "fixture-run-2".to_owned()]
    );
    assert!(run_ids.contains(&run_id));
    let parsed: ThreadSendResult = serde_json::from_value(greeting.unwrap().body.clone()).unwrap();
    let ThreadSendResult::ReceiptOnly { receipt, .. } = parsed else {
        panic!("greeting parsed as work")
    };
    assert_eq!(receipt.text, "Hello.");
    let rooms: Vec<RoomSummary> = serde_json::from_value(rooms.unwrap().body.clone()).unwrap();
    assert_eq!(rooms.len(), 1);
    assert_eq!(rooms[0].id, "fixture-room");
    assert_eq!(rooms[0].members.len(), 2);
    let computers: Vec<ComputerEntry> =
        serde_json::from_value(computers.unwrap().body.clone()).unwrap();
    assert_eq!(computers.len(), 1);
    assert_eq!(computers[0].status.computer_id, "fixture-shared-computer");
    let snapshot: BoardSnapshot = serde_json::from_value(snapshot.unwrap().body.clone()).unwrap();
    assert_eq!(snapshot.ready_ids, vec!["work-1".to_owned()]);
    assert!(snapshot.blocked_ids.is_empty());
    let item: WorkItem = serde_json::from_value(shown.unwrap().body.clone()).unwrap();
    assert_eq!(item.id, "work-1");
    assert_eq!(item.comment_count, 0);
    // Comments and history never enter the typed record.
    let encoded = serde_json::to_value(&item).unwrap();
    assert!(encoded.get("comments").is_none());
    assert!(encoded.get("history").is_none());
    // The denial carries the home's fixed board problem.
    let body = &denied.unwrap().body;
    assert_eq!(body["problem"]["code"], "access_lost");
    assert_eq!(
        body["problem"]["message"],
        "This board is only available to this computer's owner."
    );
}
#[test]
fn typescript_stage4_signatures_match_and_bind_operations() {
    let f: Value = serde_json::from_str(include_str!("fixtures/typescript-stage4.json")).unwrap();
    let mut proof = f["proof"].clone();
    proof["signature"] = Value::String(String::new());
    let proof: Proof = serde_json::from_value(proof).unwrap();
    assert_eq!(f["requests"].as_array().unwrap().len(), 14);
    assert_eq!(f["provenance"]["repository"], "ArdurAI/ardur-bot");
    assert_eq!(
        f["provenance"]["revision"],
        "12475e3af2e2b10605a27f0245f3db71b8e47f71"
    );
    assert_eq!(
        f["provenance"]["source"],
        "apps/cli/fixtures/device-operations.json"
    );
    assert_eq!(
        f["provenance"]["sha256"],
        "86fccb32d98de8d2ed20e6c26509948a9c8a67037d62e42787363afef6509466"
    );
    let copied = include_str!("fixtures/device-operations.json");
    assert_eq!(
        home_protocol::sha256(copied.as_bytes()),
        f["provenance"]["sha256"]
    );
    for r in f["requests"].as_array().unwrap() {
        let op = r["operation"].as_str().unwrap();
        let text = device_signed_text(f["instanceId"].as_str().unwrap(), &proof, op, &r["body"]);
        assert_eq!(text, r["signedText"]);
        let key = f["publicKey"].as_str().unwrap();
        let sig = r["signature"].as_str().unwrap();
        assert!(verify_device_signature(key, &text, sig));
        assert!(!verify_device_signature(key, &(text + "changed"), sig));
    }
}
