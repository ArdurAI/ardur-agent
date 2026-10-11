use home_protocol::COMPATIBILITY;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[test]
fn compatibility_manifest_matches_conformance_fixtures() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut files: Vec<_> = std::fs::read_dir(directory)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    let mut digest = Sha256::new();
    // The initial pairing/status fixtures predate the operation-vector format.
    let mut operations = BTreeSet::from(["tasks".to_owned(), "bots/list".to_owned()]);
    for path in files {
        let bytes = std::fs::read(&path).unwrap();
        assert!(!bytes.contains(&b'\r'), "Fixtures must use LF");
        digest.update(path.file_name().unwrap().to_str().unwrap().as_bytes());
        digest.update([0]);
        digest.update(&bytes);
        digest.update([0]);
        if bytes.iter().copied().find(|b| !b.is_ascii_whitespace()) != Some(b'{') {
            continue; // The reverse-direction Rust vectors use an array.
        }
        // Negative Unicode cases are intentionally not valid Rust strings.
        // Preserve other fields as raw JSON, as the conformance tests do.
        let fields: BTreeMap<String, Box<serde_json::value::RawValue>> =
            serde_json::from_slice(&bytes).unwrap();
        let Some(raw) = fields.get("requests") else {
            continue;
        };
        let requests: Vec<Value> = serde_json::from_str(raw.get()).unwrap();
        for request in requests {
            if request["operation"].is_null() {
                continue;
            }
            operations.insert(request["operation"].as_str().unwrap().to_owned());
            if request["operation"] == "rpc" {
                operations.insert(request["body"]["procedure"].as_str().unwrap().to_owned());
            }
        }
    }
    let latest: Value =
        serde_json::from_str(include_str!("fixtures/typescript-stage5.json")).unwrap();
    assert_eq!(COMPATIBILITY.schema_version, 1);
    assert_eq!(
        COMPATIBILITY.contract_revision,
        latest["provenance"]["revision"]
    );
    assert_eq!(
        COMPATIBILITY.fixture_set_sha256,
        format!("{:x}", digest.finalize()),
        "Fixture drift: review the conformance changes and update compat.json's fixtureSetSha256 (sorted filename, NUL, file bytes, NUL)."
    );
    assert_eq!(
        COMPATIBILITY.required_operations,
        operations.into_iter().collect::<Vec<_>>(),
        "Operation drift: review the client's needs and update compat.json."
    );
}
