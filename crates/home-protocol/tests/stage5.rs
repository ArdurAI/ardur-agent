use home_protocol::{Proof, canonical_json, device_signed_text, sha256};
use serde::Deserialize;
use serde_json::{Value, json};
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    instance_id: String,
    proof: Value,
    requests: Vec<Value>,
    rejected: Vec<Rejected>,
}
#[derive(Deserialize)]
struct Rejected {
    body: Box<serde_json::value::RawValue>,
}

#[test]
fn stage5_home_vectors_preserve_events_signed_bytes_and_provenance() {
    let raw = include_bytes!("fixtures/device-events.json");
    let fixture: Fixture = serde_json::from_slice(raw).unwrap();
    let provenance: Value =
        serde_json::from_str(include_str!("fixtures/device-events-provenance.json")).unwrap();
    assert_eq!(provenance["repository"], "ArdurAI/ardur-bot");
    assert_eq!(
        provenance["revision"],
        "2910f63e231ebc8fbf9c4177d4fc1681f04b3752"
    );
    assert_eq!(
        provenance["source"],
        "apps/cli/fixtures/device-operations.json"
    );
    assert_eq!(
        provenance["sha256"],
        "f5939feb81f958b8aa2bbbd38784fcd1c6dcdfdbde86440785f79a820fa5a3f3"
    );
    assert_eq!(sha256(raw), provenance["sha256"]);
    let mut proof = fixture.proof;
    proof["signature"] = json!("");
    let proof: Proof = serde_json::from_value(proof).unwrap();
    let requests = &fixture.requests;
    assert_eq!(requests.len(), 17);
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["operation"] == "events")
            .count(),
        3
    );
    for request in requests {
        assert_eq!(canonical_json(&request["body"]), request["canonicalBody"]);
        assert_eq!(
            device_signed_text(
                &fixture.instance_id,
                &proof,
                request["operation"].as_str().unwrap(),
                &request["body"]
            ),
            request["signedText"]
        );
    }
    assert_eq!(fixture.rejected.len(), 4);
    for rejected in fixture.rejected {
        assert!(matches!(
            home_protocol::parse_json(rejected.body.get().as_bytes()),
            Err(home_protocol::InvalidProtocol::InvalidUnicode)
        ));
    }
}

#[test]
fn stage5_public_signatures_bind_events_bodies_operations_and_cursors() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/typescript-stage5.json")).unwrap();
    let provenance: Value =
        serde_json::from_str(include_str!("fixtures/device-events-provenance.json")).unwrap();
    assert_eq!(fixture["provenance"], provenance);
    let mut proof = fixture["proof"].clone();
    proof["signature"] = json!("");
    let proof: Proof = serde_json::from_value(proof).unwrap();
    let key = fixture["publicKey"].as_str().unwrap();
    let requests = fixture["requests"].as_array().unwrap();
    assert_eq!(requests.len(), 17);
    for request in requests {
        let operation = request["operation"].as_str().unwrap();
        let text = device_signed_text(
            fixture["instanceId"].as_str().unwrap(),
            &proof,
            operation,
            &request["body"],
        );
        assert_eq!(text, request["signedText"]);
        let signature = request["signature"].as_str().unwrap();
        assert!(home_protocol::verify_device_signature(
            key, &text, signature
        ));
        assert!(!home_protocol::verify_device_signature(
            key,
            &(text + "changed"),
            signature
        ));
        if operation == "events" {
            let mut changed = request["body"].clone();
            changed["cursor"] = json!(0);
            let changed_text = device_signed_text(
                fixture["instanceId"].as_str().unwrap(),
                &proof,
                operation,
                &changed,
            );
            assert!(!home_protocol::verify_device_signature(
                key,
                &changed_text,
                signature
            ));
            let changed_text = device_signed_text(
                fixture["instanceId"].as_str().unwrap(),
                &proof,
                "runs/get",
                &request["body"],
            );
            assert!(!home_protocol::verify_device_signature(
                key,
                &changed_text,
                signature
            ));
        }
    }
}
