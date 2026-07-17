//! Scenario format tests: YAML round-trip and matcher defaults.

use ardur_eval::scenario::Scenario;

#[test]
fn scenario_parses_yaml() {
    let yaml = r#"
id: scenario-001
description: Agent should answer factual question
prompt: "What is the capital of France?"
expected:
  contains: ["Paris"]
  not_contains: ["London"]
  regex: "(?i)paris"
  tool_called: web_search
  cost_under: 0.01
max_tokens: 100
max_turns: 1
timeout_secs: 30
"#;

    let s = Scenario::from_yaml(yaml).expect("parses");
    assert_eq!(s.id, "scenario-001");
    assert_eq!(s.prompt, "What is the capital of France?");
    assert_eq!(s.expected.contains, vec!["Paris".to_string()]);
    assert_eq!(s.expected.not_contains, vec!["London".to_string()]);
    assert_eq!(s.expected.regex.as_deref(), Some("(?i)paris"));
    assert_eq!(s.expected.tool_called.as_deref(), Some("web_search"));
    assert_eq!(s.expected.cost_under, Some(0.01));
    assert_eq!(s.max_tokens, 100);
    assert_eq!(s.max_turns, 1);
    assert_eq!(s.timeout_secs, 30);

    // Round-trip: re-serialize and re-parse yields an equal scenario.
    let back = Scenario::from_yaml(&s.to_yaml().expect("serializes")).expect("re-parses");
    assert_eq!(s, back);
}

#[test]
fn scenario_applies_defaults() {
    // Only the required fields; everything else falls back to its default.
    let yaml = r#"
id: minimal
prompt: "hi"
"#;
    let s = Scenario::from_yaml(yaml).expect("parses");
    assert_eq!(s.description, "");
    assert_eq!(s.max_turns, 1);
    assert_eq!(s.timeout_secs, 30);
    assert_eq!(s.max_tokens, 0);
    assert!(s.expected.contains.is_empty());
    assert!(s.follow_ups.is_empty());
}
