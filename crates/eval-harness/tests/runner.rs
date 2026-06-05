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

/// The real `/chat` success body (§4.0b): nested `tokens`, `cost_usd`, a minted
/// `session_id`, and a `receipt_id`. Helper keeps the test bodies honest.
fn chat_ok(reply: &str) -> serde_json::Value {
    json!({
        "session_id": "018f5e1a-0000-7000-8000-000000000abc",
        "reply": reply,
        "tokens": { "input": 9, "output": 3 },
        "cost_usd": 0.001,
        "tools_called": [],
        "receipt_id": "018f5e1a-1111-7000-8000-000000000def",
    })
}

#[tokio::test]
async fn runner_scores_pass_correctly() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_ok("The capital is Paris.")))
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
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_ok("The capital is London.")))
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

/// The full real response shape decodes: nested `tokens` (summed and graded
/// against `max_tokens`), `cost_usd` (graded by `cost_under`), and `reply`.
#[tokio::test]
async fn runner_parses_real_chat_response_shape() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "session_id": "018f5e1a-0000-7000-8000-000000000abc",
            "reply": "The answer is 4.",
            "tokens": { "input": 120, "output": 30 },
            "cost_usd": 0.002,
            "tools_called": ["calculator"],
            "receipt_id": "018f5e1a-1111-7000-8000-000000000def",
        })))
        .mount(&server)
        .await;

    let mut s = scenario(
        "real-shape",
        Expected {
            contains: vec!["4".to_string()],
            tool_called: Some("calculator".to_string()),
            cost_under: Some(0.01),
            ..Default::default()
        },
    );
    // 150 total tokens (120 + 30) must stay within budget.
    s.max_tokens = 200;
    let client = reqwest::Client::new();
    let result = run_scenario(&client, &RunConfig::new(server.uri()), &s).await;
    assert_eq!(result.outcome, Outcome::Pass, "reply: {}", result.reply);
    assert_eq!(result.reply, "The answer is 4.");
}

/// A `502` is the runtime failing the turn — mapped to [`Outcome::Error`] with a
/// `runtime:`-prefixed message carrying the server's `{ "error": … }` detail.
#[tokio::test]
async fn runner_handles_502_runtime_error_as_outcome_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat"))
        .respond_with(ResponseTemplate::new(502).set_body_json(json!({
            "error": "cost gate denied the turn",
        })))
        .mount(&server)
        .await;

    let s = scenario("five-oh-two", Expected::default());
    let client = reqwest::Client::new();
    let result = run_scenario(&client, &RunConfig::new(server.uri()), &s).await;
    match result.outcome {
        Outcome::Error { message } => {
            assert!(message.starts_with("runtime:"), "message: {message}");
            assert!(message.contains("cost gate denied"), "message: {message}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

/// A `400` is the server rejecting the request body — a scenario *failure*
/// (not an error), surfaced with a `bad_request:`-prefixed reason.
#[tokio::test]
async fn runner_maps_400_to_fail() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "`message` is required and must be non-empty",
        })))
        .mount(&server)
        .await;

    let s = scenario("four-hundred", Expected::default());
    let client = reqwest::Client::new();
    let result = run_scenario(&client, &RunConfig::new(server.uri()), &s).await;
    match result.outcome {
        Outcome::Fail { reasons } => {
            assert_eq!(reasons.len(), 1);
            assert!(reasons[0].starts_with("bad_request:"), "{}", reasons[0]);
        }
        other => panic!("expected Fail, got {other:?}"),
    }
}

/// A multi-turn scenario omits `session_id` on the first turn and threads the
/// server-minted id through the follow-up: the mock asserts the second request
/// carries the session id the first response returned.
#[tokio::test]
async fn multi_turn_reuses_session_id() {
    use wiremock::matchers::body_partial_json;

    let server = MockServer::start().await;
    let minted = "018f5e1a-2222-7000-8000-000000000999";

    // First turn: no session_id in the body → server mints `minted`.
    Mock::given(method("POST"))
        .and(path("/chat"))
        .and(body_partial_json(json!({ "message": "remember teal" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "session_id": minted,
            "reply": "noted",
            "tokens": { "input": 4, "output": 1 },
            "cost_usd": 0.0,
            "tools_called": [],
            "receipt_id": "018f5e1a-3333-7000-8000-000000000aaa",
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    // Follow-up turn: must echo the minted session_id back. This mock only
    // matches when the body carries it, so a runner that dropped the session id
    // would fall through to no match (→ 404 → Outcome::Error) and fail the test.
    Mock::given(method("POST"))
        .and(path("/chat"))
        .and(body_partial_json(json!({
            "message": "what colour?",
            "session_id": minted,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "session_id": minted,
            "reply": "teal",
            "tokens": { "input": 6, "output": 1 },
            "cost_usd": 0.0,
            "tools_called": [],
            "receipt_id": "018f5e1a-4444-7000-8000-000000000bbb",
        })))
        .mount(&server)
        .await;

    let mut s = scenario(
        "multi-turn",
        Expected {
            contains: vec!["teal".to_string()],
            ..Default::default()
        },
    );
    s.prompt = "remember teal".to_string();
    s.follow_ups = vec!["what colour?".to_string()];
    let client = reqwest::Client::new();
    let result = run_scenario(&client, &RunConfig::new(server.uri()), &s).await;
    assert_eq!(result.outcome, Outcome::Pass, "reply: {}", result.reply);
    assert_eq!(result.reply, "teal", "matchers grade the final turn");
}
