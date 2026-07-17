//! Output-format tests: render a small result set in each format and assert the
//! shape (valid JSON, JUnit counts, Markdown table).

use ardur_eval::output::{Format, Summary, render};
use ardur_eval::runner::{Outcome, ScenarioResult};

fn results() -> Vec<ScenarioResult> {
    vec![
        ScenarioResult {
            id: "pass-one".to_string(),
            description: "a passing case".to_string(),
            outcome: Outcome::Pass,
            reply: "Paris".to_string(),
            duration_ms: 5,
        },
        ScenarioResult {
            id: "fail-one".to_string(),
            description: "a failing case".to_string(),
            outcome: Outcome::Fail {
                reasons: vec!["expected reply to contain \"Paris\"".to_string()],
            },
            reply: "London".to_string(),
            duration_ms: 7,
        },
    ]
}

#[test]
fn summary_tallies() {
    let s = Summary::of(&results());
    assert_eq!(s.passed, 1);
    assert_eq!(s.failed, 1);
    assert_eq!(s.errored, 0);
    assert_eq!(s.total(), 2);
    assert!(!s.is_green());
}

#[test]
fn json_is_valid() {
    let out = render(&results(), Format::Json);
    let parsed: serde_json::Value = serde_json::from_str(&out).expect("valid json");
    assert_eq!(parsed["summary"]["passed"], 1);
    assert_eq!(parsed["summary"]["failed"], 1);
    assert_eq!(parsed["results"].as_array().unwrap().len(), 2);
}

#[test]
fn junit_has_counts_and_failure() {
    let out = render(&results(), Format::Junit);
    assert!(out.contains("tests=\"2\""), "{out}");
    assert!(out.contains("failures=\"1\""), "{out}");
    assert!(out.contains("<failure"), "{out}");
}

#[test]
fn markdown_has_table() {
    let out = render(&results(), Format::Markdown);
    assert!(out.contains("| Scenario | Status"), "{out}");
    assert!(out.contains("pass-one"), "{out}");
    assert!(out.contains("❌ fail"), "{out}");
}
