//! Runner tests: drive `run_scenario` against a `wiremock` stand-in for the
//! ardur-server `/chat` endpoint and assert the graded outcome.

use ardur_eval::runner::{Outcome, RunConfig, run_scenario};
use ardur_eval::scenario::{Expected, Scenario};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn scenario(id: &str, expected: Expected) -> Scenario {
    Scenario {
        id: id.to_string(),
        description: "test".to_string(),
        prompt: "hi".to_string(),
        expected,
        max_tokens: 0,
        max_turns: 1,
        timeout_secs: 30,
        follow_ups: Vec::new(),
    }
}

#[tokio::test]
async fn runner_scores_pass_correctly() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "reply": "The capital is Paris.",
            "tokens": 12,
            "cost_usd": 0.001,
            "tools_called": [],
        })))
        .mount(&server)
        .await;

    let s = scenario(
        "pass-case",
        Expected {
            contains: vec!["Paris".to_string()],
            ..Default::default()
        },
    );
    let client = reqwest::Client::new();
    let result = run_scenario(&client, &RunConfig::new(server.uri()), &s).await;
    assert_eq!(result.outcome, Outcome::Pass, "reply: {}", result.reply);
}

#[tokio::test]
async fn runner_scores_fail_with_reason() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "reply": "The capital is London.",
        })))
        .mount(&server)
        .await;

    let s = scenario(
        "fail-case",
        Expected {
            contains: vec!["Paris".to_string()],
            ..Default::default()
        },
    );
    let client = reqwest::Client::new();
    let result = run_scenario(&client, &RunConfig::new(server.uri()), &s).await;
    match result.outcome {
        Outcome::Fail { reasons } => {
            assert_eq!(reasons.len(), 1);
            assert!(reasons[0].contains("Paris"));
        }
        other => panic!("expected Fail, got {other:?}"),
    }
}

#[tokio::test]
async fn runner_reports_error_on_non_2xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let s = scenario("err-case", Expected::default());
    let client = reqwest::Client::new();
    let result = run_scenario(&client, &RunConfig::new(server.uri()), &s).await;
    assert!(matches!(result.outcome, Outcome::Error { .. }));
}
