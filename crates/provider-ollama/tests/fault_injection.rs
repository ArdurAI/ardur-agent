//! Fault-injection tests for the resilience layer wired into
//! `OllamaProvider` (`ardur-resilience`).

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use ardur_provider_ollama::{OllamaConfig, OllamaProvider};
use ardur_provider_runtime::{ChatMessage, CompletionRequest, ModelId, Provider, ProviderError};
use ardur_resilience::{CircuitBreakerConfig, RetryPolicy};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fast_retry_policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        initial_backoff_ms: 1,
        max_backoff_ms: 2,
        backoff_multiplier: 2,
    }
}

fn req() -> CompletionRequest {
    CompletionRequest::new(
        vec![ChatMessage::user("ping")],
        ModelId::new("llama3.2"),
        16,
    )
}

#[tokio::test]
async fn transient_500_is_retried_into_success() {
    let server = MockServer::start().await;
    let calls = std::sync::Arc::new(AtomicU32::new(0));
    let calls_clone = calls.clone();

    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(move |_: &wiremock::Request| {
            let n = calls_clone.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                ResponseTemplate::new(500).set_body_json(serde_json::json!({"error": "hiccup"}))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"role": "assistant", "content": "pong"},
                    "done_reason": "stop",
                    "done": true,
                    "prompt_eval_count": 1,
                    "eval_count": 1
                }))
            }
        })
        .mount(&server)
        .await;

    let provider = OllamaProvider::new(
        OllamaConfig::new()
            .base_url(server.uri())
            .retry_policy(fast_retry_policy()),
    );

    let resp = provider
        .complete(req())
        .await
        .expect("the third attempt succeeds");
    assert_eq!(resp.content, "pong");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn permanent_401_is_never_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;

    let provider = OllamaProvider::new(
        OllamaConfig::new()
            .base_url(server.uri())
            .api_key("sk-test")
            .retry_policy(fast_retry_policy()),
    );

    let err = provider.complete(req()).await.expect_err("401 is fatal");
    assert!(matches!(err, ProviderError::Unauthorized));
}

#[tokio::test]
async fn open_breaker_fails_fast_without_any_further_network_calls() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(500))
        .expect(2)
        .mount(&server)
        .await;

    let provider = OllamaProvider::new(
        OllamaConfig::new()
            .base_url(server.uri())
            .retry_policy(RetryPolicy::none())
            .circuit_breaker(CircuitBreakerConfig {
                failure_threshold: 2,
                open_duration: Duration::from_secs(60),
            }),
    );

    for _ in 0..2 {
        let err = provider.complete(req()).await.expect_err("500 is an error");
        assert!(matches!(err, ProviderError::Upstream(_)));
    }

    let err = provider.complete(req()).await.expect_err("breaker is open");
    assert!(
        matches!(&err, ProviderError::Upstream(msg) if msg.contains("circuit breaker open")),
        "got {err:?}"
    );
}

#[tokio::test]
async fn connection_refused_is_retried_and_eventually_surfaces() {
    // Nothing is listening on this port — every attempt fails to connect.
    let provider = OllamaProvider::new(
        OllamaConfig::new()
            .base_url("http://127.0.0.1:1")
            .retry_policy(fast_retry_policy())
            .circuit_breaker(CircuitBreakerConfig {
                failure_threshold: 100,
                open_duration: Duration::from_secs(60),
            }),
    );

    let err = provider
        .complete(req())
        .await
        .expect_err("nothing is listening");
    assert!(
        matches!(&err, ProviderError::Upstream(msg) if msg.contains("is the daemon running")),
        "got {err:?}"
    );
}
