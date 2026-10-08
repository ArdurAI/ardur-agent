use base64::Engine;
use home_protocol::{InvalidProtocol, canonical_json, decode_pairing_code, parse_json};
use serde_json::Value;

#[test]
fn accepted_input_domain_vectors() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/input-domain.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let raw = case["raw"].as_str().unwrap();
        match parse_json(raw.as_bytes()) {
            Ok(value) => {
                assert_eq!(case["accepted"], true, "{}", case["name"]);
                assert_eq!(canonical_json(&value), case["canonical"]);
            }
            Err(error) => {
                assert_eq!(case["accepted"], false, "{}", case["name"]);
                assert!(matches!(
                    (case["error"].as_str().unwrap(), error),
                    ("unicode", InvalidProtocol::InvalidUnicode)
                        | ("data", InvalidProtocol::InvalidData)
                ));
            }
        }
    }
    let normalized = fixture["normalization"]["wire"].as_str().unwrap();
    assert_eq!(
        canonical_json(&parse_json(normalized.as_bytes()).unwrap()),
        fixture["normalization"]["canonical"]
    );
}
#[test]
fn pairing_rejects_unpaired_surrogates_in_values_and_keys() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/typescript.json")).unwrap();
    let original = serde_json::to_string(&fixture["payload"]).unwrap();
    let name = serde_json::to_string(&fixture["payload"]["homeName"]).unwrap();
    for escape in [r"\ud800", r"\udfff"] {
        for raw in [
            original.replace(&name, &format!("\"{escape}\"")),
            original.replace("\"homeName\"", &format!("\"{escape}\"")),
        ] {
            for code in [
                raw.clone(),
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw.as_bytes()),
            ] {
                let error = decode_pairing_code(&code).err().unwrap();
                assert!(matches!(error, InvalidProtocol::InvalidUnicode));
                assert_eq!(
                    error.to_string(),
                    "JSON strings must contain well-formed Unicode; unpaired surrogates are not allowed."
                );
                assert!(!error.to_string().contains(escape));
            }
        }
    }
    let good = original.replace(&name, r#""\ud83d\ude00""#);
    assert_eq!(decode_pairing_code(&good).unwrap().home_name, "😀");
}
