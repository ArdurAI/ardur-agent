//! Export only synthetic public conformance material; never private keys.
use home_protocol::*;
use serde_json::{Value, json};
fn main() {
    let f: Value = serde_json::from_str(include_str!("../tests/fixtures/typescript.json")).unwrap();
    let p: PairingPayload = serde_json::from_value(f["payload"].clone()).unwrap();
    let k = DeviceKeys::generate().unwrap();
    let text = pairing_signed_text(&p, &k.public_key, &k.presence_public_key);
    let mut vectors = vec![
        json!({"kind":"pairing","payload":p,"publicKey":k.public_key,"presencePublicKey":k.presence_public_key,"text":text,"signature":k.sign(&text).unwrap()}),
    ];
    let proof:Proof=serde_json::from_value(json!({"grantId":f["proof"]["grantId"],"nonce":f["proof"]["nonce"],"timestamp":f["proof"]["timestamp"],"signature":""})).unwrap();
    for r in f["requests"].as_array().unwrap() {
        let text = device_signed_text(&p.instance_id, &proof, "rpc", &r["body"]);
        vectors.push(json!({"kind":"request","instanceId":p.instance_id,"proof":proof,"operation":"rpc","body":r["body"],"publicKey":k.public_key,"text":text,"signature":k.sign(&text).unwrap()}));
    }
    let stage3: Value =
        serde_json::from_str(include_str!("../tests/fixtures/typescript-stage3.json")).unwrap();
    let mut proof = stage3["proof"].clone();
    proof["signature"] = json!("");
    let proof: Proof = serde_json::from_value(proof).unwrap();
    for r in stage3["requests"].as_array().unwrap() {
        let operation = r["operation"].as_str().unwrap();
        let instance = stage3["instanceId"].as_str().unwrap();
        let text = device_signed_text(instance, &proof, operation, &r["body"]);
        vectors.push(json!({"kind":"request","instanceId":instance,"proof":proof,"operation":operation,"body":r["body"],"publicKey":k.public_key,"text":text,"signature":k.sign(&text).unwrap()}));
    }
    println!("{}", serde_json::to_string_pretty(&vectors).unwrap());
}
