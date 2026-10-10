use home_protocol::*;
use serde_json::Value;
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
