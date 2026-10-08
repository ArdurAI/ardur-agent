use home_protocol::canonical_json;
use serde_json::Value;
#[test]
fn javascript_float_parse_boundaries() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/numbers.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let raw = case["raw"].as_str().unwrap();
        let v: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(canonical_json(&v), case["canonical"], "input {raw}");
    }
}
