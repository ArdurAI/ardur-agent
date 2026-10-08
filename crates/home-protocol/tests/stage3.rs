use home_protocol::*;
use serde::Deserialize;
use serde_json::Value;
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixture {
    instance_id: String,
    proof: Value,
    requests: Vec<Request>,
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
struct Rejected {
    body: Box<serde_json::value::RawValue>,
}
#[test]
fn copied_home_vectors_preserve_every_signed_byte_and_refuse_surrogates() {
    let f: Fixture = serde_json::from_str(include_str!("fixtures/device-operations.json")).unwrap();
    let mut proof = f.proof;
    proof["signature"] = Value::String(String::new());
    let proof: Proof = serde_json::from_value(proof).unwrap();
    assert_eq!(f.requests.len(), 8);
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
fn typescript_stage3_signatures_match_and_bind_operations() {
    let f: Value = serde_json::from_str(include_str!("fixtures/typescript-stage3.json")).unwrap();
    let mut proof = f["proof"].clone();
    proof["signature"] = Value::String(String::new());
    let proof: Proof = serde_json::from_value(proof).unwrap();
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
