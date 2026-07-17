//! Matcher tests against the pure `grade` function — no HTTP needed.

use ardur_eval::runner::grade;
use ardur_eval::scenario::Expected;

fn expect(e: Expected, reply: &str) -> Vec<String> {
    grade(&e, reply, None, None, &[], 0)
}

#[test]
fn matcher_contains() {
    let e = Expected {
        contains: vec!["Paris".to_string()],
        ..Default::default()
    };
    assert!(expect(e.clone(), "The capital is Paris.").is_empty());
    assert_eq!(expect(e, "The capital is London.").len(), 1);
}

#[test]
fn matcher_not_contains() {
    let e = Expected {
        not_contains: vec!["London".to_string()],
        ..Default::default()
    };
    assert!(expect(e.clone(), "The capital is Paris.").is_empty());
    let reasons = expect(e, "The capital is London.");
    assert_eq!(reasons.len(), 1);
    assert!(reasons[0].contains("NOT contain"));
}

#[test]
fn matcher_regex() {
    let e = Expected {
        regex: Some("(?i)\\bparis\\b".to_string()),
        ..Default::default()
    };
    assert!(expect(e.clone(), "It is PARIS, of course.").is_empty());
    assert_eq!(expect(e, "It is Lyon.").len(), 1);
}

#[test]
fn matcher_tool_called() {
    let e = Expected {
        tool_called: Some("web_search".to_string()),
        ..Default::default()
    };
    let called = vec!["web_search".to_string()];
    assert!(grade(&e, "ok", None, None, &called, 0).is_empty());
    assert_eq!(grade(&e, "ok", None, None, &[], 0).len(), 1);
}

#[test]
fn matcher_cost_under() {
    let e = Expected {
        cost_under: Some(0.01),
        ..Default::default()
    };
    assert!(grade(&e, "ok", None, Some(0.005), &[], 0).is_empty());
    // At or above the threshold fails; missing cost also fails.
    assert_eq!(grade(&e, "ok", None, Some(0.02), &[], 0).len(), 1);
    assert_eq!(grade(&e, "ok", None, None, &[], 0).len(), 1);
}

#[test]
fn matcher_max_tokens() {
    let e = Expected::default();
    // Under budget passes; over budget reports one failure.
    assert!(grade(&e, "ok", Some(40), None, &[], 100).is_empty());
    assert_eq!(grade(&e, "ok", Some(150), None, &[], 100).len(), 1);
}
