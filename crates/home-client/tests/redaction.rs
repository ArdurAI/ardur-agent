use home_client::{CommandResult, human_text, safe_output};
use serde_json::{Value, json};
const FIXTURE: &str = include_str!("fixtures/redaction.json");

#[test]
fn every_oracle_case_matches_before_json_or_human_output() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let expected = case["expected"].as_str().unwrap();
        assert_eq!(safe_output(input), expected, "{}", case["name"]);
        let mut result = CommandResult::empty(input);
        result.reply_text = input.into();
        result.failure_reason = Some(input.into());
        result.run_id = Some(input.into());
        result.task_id = Some(input.into());
        result.bot = serde_json::from_value(json!({"id": input, "name": input})).ok();
        result.data = json!({input: [input, {"nested": input}]});
        let encoded = result.json();
        for field in ["command", "replyText", "failureReason", "runId", "taskId"] {
            assert_eq!(encoded[field], expected, "{}: {field}", case["name"]);
        }
        assert_eq!(encoded["bot"]["id"], expected);
        assert_eq!(encoded["bot"]["name"], expected);
        assert_eq!(encoded["data"][expected][0], expected);
        assert_eq!(encoded["data"][expected][1]["nested"], expected);
        assert_eq!(result.human(), expected);
        assert_eq!(human_text(&result.human()), human_text(expected));
        result.failure_reason = None;
        assert_eq!(result.human(), expected);
        result.reply_text.clear();
        result.command = "runs show".into();
        result.data = json!({"value": input});
        let human: Value = serde_json::from_str(&result.human()).unwrap();
        assert_eq!(human, json!({"value": expected}));
    }
    let saved = &fixture["savedAnswer"];
    assert_eq!(
        safe_output(saved["input"].as_str().unwrap()),
        saved["expected"].as_str().unwrap()
    );
}
