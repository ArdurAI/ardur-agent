use ardur_eval::home::{HomeScenario, grade_evidence};
use ardur_eval::output::{Format, Summary, render};
use ardur_eval::runner::{Outcome, ScenarioResult};
use ardur_eval::scenario::Expected;
use ardur_eval::transcript::Transcript;
use serde_json::json;

fn grade(expected: &Expected, reply: &str) -> Outcome {
    grade_evidence(expected, reply, None, None, None, 0)
}
#[test]
fn exact_contains_and_regex_preserve_boundaries() {
    let expected = Expected {
        exact: Some("ok ✓\n".into()),
        ..Default::default()
    };
    assert_eq!(grade(&expected, "ok ✓\n"), Outcome::Pass);
    for reply in ["ok ✓", "OK ✓\n", "prefix ok ✓\n", "ok ✓\n "] {
        assert!(matches!(grade(&expected, reply), Outcome::Fail { .. }));
    }
    let contains = Expected {
        contains: vec!["cat".into()],
        ..Default::default()
    };
    assert_eq!(grade(&contains, "catalog"), Outcome::Pass);
    assert!(matches!(grade(&contains, "CAT"), Outcome::Fail { .. }));
    let regex = Expected {
        regex: Some(r"\bcat\b".into()),
        ..Default::default()
    };
    assert_eq!(grade(&regex, "a cat!"), Outcome::Pass);
    assert!(matches!(grade(&regex, "catalog"), Outcome::Fail { .. }));
    assert_eq!(
        grade(
            &Expected {
                exact: Some(String::new()),
                ..Default::default()
            },
            ""
        ),
        Outcome::Pass
    );
}
#[test]
fn evidence_absence_is_different_from_an_empty_authoritative_record() {
    let expected = Expected {
        tool_called: Some("search".into()),
        cost_under: Some(0.1),
        ..Default::default()
    };
    let outcome = grade_evidence(&expected, "tool search cost 0", None, None, None, 10);
    assert!(matches!(outcome, Outcome::Unavailable { reasons } if reasons.len() == 3));
    assert!(matches!(
        grade_evidence(&expected, "ok", Some(5), Some(0.01), Some(&[]), 10),
        Outcome::Fail { .. }
    ));
    assert_eq!(
        grade_evidence(
            &expected,
            "ok",
            Some(10),
            Some(0.01),
            Some(&["search".into()]),
            10
        ),
        Outcome::Pass
    );
    assert!(matches!(
        grade_evidence(&expected, "ok", None, Some(f64::NAN), None, 0),
        Outcome::Unavailable { .. }
    ));
    assert!(matches!(
        grade_evidence(
            &expected,
            "ok",
            Some(10),
            Some(0.1),
            Some(&["search".into()]),
            10
        ),
        Outcome::Fail { .. }
    ));
    let mismatch = Expected {
        exact: Some("expected".into()),
        ..expected
    };
    assert!(matches!(grade(&mismatch, "other"), Outcome::Fail { reasons } if reasons.len() == 3));
}
#[test]
fn malformed_scenarios_are_rejected_before_execution() {
    let valid = "id: case\nprompt: hello\ntarget: {kind: bot, bot: fixture}\n";
    assert!(HomeScenario::from_yaml(valid).is_ok());
    for bad in [
        "[",
        "id: case",
        "- hello",
        "id: case\nprompt: hello\n",
        "id: case\nprompt: hello\ntarget: {kind: both, bot: fixture}",
    ] {
        assert!(HomeScenario::from_yaml(bad).is_err());
    }
    for suffix in [
        "typo: value\n",
        "expected: {contians: [hello]}\n",
        "expected: {regex: '['}\n",
        "max_turns: 0\n",
        "timeout_secs: 0\n",
        "follow_ups: [second]\n",
        "expected: {cost_under: .nan}\n",
    ] {
        assert!(
            HomeScenario::from_yaml(&format!("{valid}{suffix}")).is_err(),
            "{suffix}"
        );
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.yaml"), valid).unwrap();
    std::fs::write(dir.path().join("b.yaml"), valid).unwrap();
    assert!(HomeScenario::load(dir.path()).is_err());
}
#[test]
fn transcript_recovers_synced_records_and_ignores_only_a_torn_tail() {
    let dir = tempfile::tempdir().unwrap();
    let mut transcript = Transcript::create(dir.path()).unwrap();
    transcript.append(json!({"kind":"request","requestId":"stable-request-123","prompt":"password=fixture-secret\nAuthorization: Bearer fixture-auth"})).unwrap();
    transcript
        .append(json!({"kind":"admission","runId":"run-1","taskId":"task-1"}))
        .unwrap();
    drop(transcript);
    let path = std::fs::read_dir(dir.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"kind\":")
        .unwrap();
    let records = Transcript::recover(&path).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["requestId"], "stable-request-123");
    assert_eq!(records[1]["runId"], "run-1");
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("fixture-secret"));
    assert!(!raw.contains("fixture-auth"));
    std::fs::write(&path, "invalid\n").unwrap();
    assert!(Transcript::recover(&path).is_err());
}
#[test]
fn report_totals_and_all_fields_are_redacted_before_serialization() {
    let outcomes = [
        Outcome::Pass,
        Outcome::Fail {
            reasons: vec!["password=fixture-failure".into()],
        },
        Outcome::Unavailable {
            reasons: vec!["Authorization: Bearer fixture-missing".into()],
        },
        Outcome::Error {
            message: "api_key=fixture-error".into(),
        },
    ];
    let results: Vec<_> = outcomes.into_iter().map(|outcome| ScenarioResult { home_update_required: false, id: "password=fixture-id".into(), description: "api_key=fixture-description".into(), outcome, reply: "password=fixture-reply\n-----BEGIN PRIVATE KEY-----\nfixture-pem\n-----END PRIVATE KEY-----".into(), duration_ms: 1 }).collect();
    let summary = Summary::of(&results);
    assert_eq!(
        (
            summary.total(),
            summary.passed,
            summary.failed,
            summary.unavailable,
            summary.errored
        ),
        (4, 1, 1, 1, 1)
    );
    assert!(!summary.is_green());
    let report: serde_json::Value = serde_json::from_str(&render(&results, Format::Json)).unwrap();
    assert_eq!(
        report["summary"],
        json!({"total":4,"passed":1,"failed":1,"unavailable":1,"errored":1})
    );
    let xml = render(&results, Format::Junit);
    for attr in [
        "tests=\"4\"",
        "failures=\"1\"",
        "errors=\"1\"",
        "skipped=\"1\"",
    ] {
        assert!(xml.contains(attr));
    }
    assert!(xml.contains("<skipped"));
    for format in [Format::Json, Format::Junit, Format::Markdown] {
        let text = render(&results, format);
        for secret in [
            "fixture-id",
            "fixture-description",
            "fixture-reply",
            "fixture-failure",
            "fixture-missing",
            "fixture-error",
            "fixture-pem",
        ] {
            assert!(!text.contains(secret), "{text}");
        }
    }
}
