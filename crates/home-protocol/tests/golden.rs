use home_protocol::*;
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/typescript.json")).unwrap()
}
#[test]
fn typescript_vectors_and_rsa_home_proof() {
    let f = fixture();
    assert_eq!(f["homeKeyAlgorithm"], "rsa");
    let payload = decode_pairing_code(f["code"].as_str().unwrap()).unwrap();
    assert_eq!(serde_json::to_value(&payload).unwrap(), f["payload"]);
    let text = pairing_signed_text(
        &payload,
        f["publicKey"].as_str().unwrap(),
        f["presencePublicKey"].as_str().unwrap(),
    );
    assert_eq!(text, f["pairingText"]);
    assert!(verify_device_signature(
        f["publicKey"].as_str().unwrap(),
        &text,
        f["pairingSignature"].as_str().unwrap()
    ));
    let proof: Proof = serde_json::from_value(json!({"grantId":f["proof"]["grantId"],"nonce":f["proof"]["nonce"],"timestamp":f["proof"]["timestamp"],"signature":""})).unwrap();
    for r in f["requests"].as_array().unwrap() {
        assert_eq!(canonical_json(&r["body"]), r["canonical"]);
        let text = device_signed_text(&payload.instance_id, &proof, "rpc", &r["body"]);
        assert_eq!(text, r["text"]);
        assert!(verify_device_signature(
            f["publicKey"].as_str().unwrap(),
            &text,
            r["signature"].as_str().unwrap()
        ));
        assert!(!verify_device_signature(
            f["publicKey"].as_str().unwrap(),
            &(text + "x"),
            r["signature"].as_str().unwrap()
        ));
    }
    let identity: Identity = serde_json::from_value(f["identity"].clone()).unwrap();
    let now = f["validAt"].as_i64().unwrap();
    assert_eq!(
        home_signed_text(
            &payload.instance_id,
            &payload.fingerprint,
            f["clientChallenge"].as_str().unwrap()
        ),
        f["homeText"]
    );
    assert!(verify_home(
        &payload.pins(),
        f["clientChallenge"].as_str().unwrap(),
        &identity,
        now
    ));
    assert!(!verify_home(&payload.pins(), "changed", &identity, now));
    assert!(!verify_home(
        &payload.pins(),
        f["clientChallenge"].as_str().unwrap(),
        &identity,
        f["expiredAt"].as_i64().unwrap()
    ));
    for field in ["instanceId", "fingerprint", "certificateFingerprint"] {
        let mut p = f["payload"].clone();
        p[field] = json!(if field == "instanceId" {
            "changed".into()
        } else {
            "0".repeat(64)
        });
        let p: PairingPayload = serde_json::from_value(p).unwrap();
        assert!(!verify_home(
            &p.pins(),
            f["clientChallenge"].as_str().unwrap(),
            &identity,
            now
        ));
    }
}
#[test]
fn two_keys_and_der_signatures_bind_all_fields() {
    let k = DeviceKeys::generate().unwrap();
    assert_ne!(k.public_key, k.presence_public_key);
    let proof = Proof {
        grant_id: "grant".into(),
        nonce: "n".repeat(43),
        timestamp: 1791429078000,
        signature: String::new(),
    };
    let body = json!({"procedure":"bots/list","input":{}});
    let text = device_signed_text("home", &proof, "rpc", &body);
    let signature = k.sign(&text).unwrap();
    assert!(verify_device_signature(&k.public_key, &text, &signature));
    assert!(!verify_device_signature(
        &k.presence_public_key,
        &text,
        &signature
    ));
    for other in [
        device_signed_text("other", &proof, "rpc", &body),
        device_signed_text("home", &proof, "tasks", &body),
        device_signed_text("home", &proof, "rpc", &json!({})),
    ] {
        assert!(!verify_device_signature(&k.public_key, &other, &signature));
    }
}
#[test]
fn strict_pairing_input_and_nonce_boundaries() {
    let f = fixture();
    for field in ["extra", "fingerprint", "version", "hints", "challenge"] {
        let mut p = f["payload"].clone();
        p[field] = match field {
            "version" => json!(2),
            "hints" => json!(["http://home.test"]),
            _ => json!("bad"),
        };
        assert!(decode_pairing_code(&p.to_string()).is_err());
    }
    assert!(decode_pairing_code(&"x".repeat(16385)).is_err());
    let mut i: Identity = serde_json::from_value(f["identity"].clone()).unwrap();
    i.nonce = Some("n".repeat(32));
    i.timestamp = Some(100001);
    assert!(valid_nonce(&i, 160001));
    assert!(!valid_nonce(&i, 160002));
    i.timestamp = Some(0);
    assert!(!valid_nonce(&i, 0));
}
