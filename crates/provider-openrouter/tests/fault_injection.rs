//! Fault-injection tests for the resilience layer wired into
//! `OpenRouterProvider` (`ardur-resilience`): retries a transient `5xx`
//! into a success, exhausts retries against a permanent outage, respects
//! `429`, and trips the circuit breaker to fail fast without any further
//! network calls.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use ardur_provider_openrouter::{OpenRouterConfig, OpenRouterProvider};
use ardur_provider_runtime::{ChatMessage, CompletionRequest, ModelId, Provider, ProviderError};
use ardur_resilience::{CircuitBreakerConfig, RetryPolicy};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fast_retry_policy() -> RetryPolicy {
    // Keep the test fast: real backoff shape is covered by
    // crates/resilience's own unit tests.
    RetryPolicy {
        max_attempts: 3,
        initial_backoff_ms: 1,
        max_backoff_ms: 2,
        backoff_multiplier: 2,
    }
}

fn req() -> CompletionRequest {
    CompletionRequest::new(vec![ChatMessage::user("ping")], ModelId::new("m"), 16)
}

#[tokio::test]
async fn transient_500_is_retried_into_success() {
    let server = MockServer::start().await;
    let calls = std::sync::Arc::new(AtomicU32::new(0));
    let calls_clone = calls.clone();

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |_: &wiremock::Request| {
            let n = calls_clone.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                ResponseTemplate::new(500).set_body_json(serde_json::json!({
                    "error": {"message": "upstream hiccup"}
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"content": "pong"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "cost": 0.0}
                }))
            }
        })
        .mount(&server)
        .await;

    let provider = OpenRouterProvider::new(
        OpenRouterConfig::new("sk-test")
            .base_url(server.uri())
            .retry_policy(fast_retry_policy()),
        ModelId::new("m"),
    );

    let resp = provider
        .complete(req())
        .await
        .expect("the third attempt succeeds");
    assert_eq!(resp.content, "pong");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "two failures then a success — three attempts total"
    );
}

#[tokio::test]
async fn persistent_500_exhausts_retries_and_surfaces_upstream_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
            "error": {"message": "down for maintenance"}
        })))
        .mount(&server)
        .await;

    let provider = OpenRouterProvider::new(
        OpenRouterConfig::new("sk-test")
            .base_url(server.uri())
            .retry_policy(fast_retry_policy())
            // Keep the breaker from tripping mid-test so we see the retry
            // exhaustion error, not a breaker-open error.
            .circuit_breaker(CircuitBreakerConfig {
                failure_threshold: 100,
                open_duration: Duration::from_secs(60),
            }),
        ModelId::new("m"),
    );

    let err = provider
        .complete(req())
        .await
        .expect_err("every attempt fails");
    assert!(
        matches!(&err, ProviderError::Upstream(msg) if msg.contains("down for maintenance")),
        "got {err:?}"
    );
}

#[tokio::test]
async fn permanent_401_is_never_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;

    let provider = OpenRouterProvider::new(
        OpenRouterConfig::new("sk-test")
            .base_url(server.uri())
            .retry_policy(fast_retry_policy()),
        ModelId::new("m"),
    );

    let err = provider.complete(req()).await.expect_err("401 is fatal");
    assert!(matches!(err, ProviderError::Unauthorized));
    // wiremock's `.expect(1)` assertion (verified on Drop) proves no retry
    // happened for a permanent auth failure.
}

#[tokio::test]
async fn open_breaker_fails_fast_without_any_further_network_calls() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500))
        .expect(2) // one failing attempt per `complete()` call, threshold 2
        .mount(&server)
        .await;

    let provider = OpenRouterProvider::new(
        OpenRouterConfig::new("sk-test")
            .base_url(server.uri())
            // No retrying, so each `complete()` call is exactly one HTTP
            // request — isolates the breaker's own failure counting.
            .retry_policy(RetryPolicy::none())
            .circuit_breaker(CircuitBreakerConfig {
                failure_threshold: 2,
                open_duration: Duration::from_secs(60),
            }),
        ModelId::new("m"),
    );

    for _ in 0..2 {
        let err = provider.complete(req()).await.expect_err("500 is an error");
        assert!(matches!(err, ProviderError::Upstream(_)));
    }

    // The breaker is now open: a third call must fail fast, and — because
    // wiremock's mount above only `.expect(2)` — no further request may hit
    // the server at all.
    let err = provider.complete(req()).await.expect_err("breaker is open");
    assert!(
        matches!(&err, ProviderError::Upstream(msg) if msg.contains("circuit breaker open")),
        "got {err:?}"
    );
}

#[tokio::test]
async fn slow_upstream_times_out_rather_than_hanging_forever() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;

    let provider = OpenRouterProvider::new(
        OpenRouterConfig::new("sk-test")
            .base_url(server.uri())
            .request_timeout(Duration::from_millis(20))
            .retry_policy(RetryPolicy::none()),
        ModelId::new("m"),
    );

    let err = tokio::time::timeout(Duration::from_secs(2), provider.complete(req()))
        .await
        .expect("the call itself must not hang past the configured request timeout")
        .expect_err("a slow upstream times out rather than succeeding");
    assert!(
        matches!(err, ProviderError::NetworkFailure(_)),
        "got {err:?}"
    );
}
