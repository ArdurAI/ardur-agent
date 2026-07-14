//! Fault-injection tests for the resilience layer wired into `SlackAdapter`'s
//! outbound `chat.postMessage` (`ardur-resilience`).

mod common;

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use ardur_resilience::circuit_breaker::CircuitBreakerConfig;
use ardur_resilience::retry::RetryPolicy;
use ardur_slack_adapter::{SlackAdapter, SlackError};
use secrecy::SecretString;
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

fn adapter(base_url: String) -> SlackAdapter {
    SlackAdapter::new(
        SecretString::from(common::BOT_TOKEN.to_string()),
        SecretString::from(common::SIGNING_SECRET.to_string()),
        common::APP_ID.to_string(),
    )
    .with_base_url(base_url)
    .with_retry_policy(fast_retry_policy())
}

#[tokio::test]
async fn transient_500_is_retried_into_success() {
    let server = MockServer::start().await;
    let calls = std::sync::Arc::new(AtomicU32::new(0));
    let calls_clone = calls.clone();

    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .respond_with(move |_: &wiremock::Request| {
            let n = calls_clone.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                ResponseTemplate::new(500)
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "ok": true, "ts": "123.456" }))
            }
        })
        .mount(&server)
        .await;

    let ts = adapter(server.uri())
        .post_message("C1", "hi", None)
        .await
        .expect("third attempt succeeds");
    assert_eq!(ts, "123.456");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn business_error_is_never_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "ok": false, "error": "not_in_channel" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let err = adapter(server.uri())
        .post_message("C1", "hi", None)
        .await
        .expect_err("not_in_channel is fatal");
    assert!(matches!(err, SlackError::Forbidden));
}

#[tokio::test]
async fn open_breaker_fails_fast_without_any_further_network_calls() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .respond_with(ResponseTemplate::new(500))
        .expect(2)
        .mount(&server)
        .await;

    let a = adapter(server.uri())
        .with_retry_policy(RetryPolicy::none())
        .with_circuit_breaker(CircuitBreakerConfig {
            failure_threshold: 2,
            open_duration: Duration::from_secs(60),
        });

    for _ in 0..2 {
        a.post_message("C1", "hi", None)
            .await
            .expect_err("500 is an error");
    }

    let err = a
        .post_message("C1", "hi", None)
        .await
        .expect_err("breaker is open");
    assert!(
        matches!(&err, SlackError::Upstream(msg) if msg.contains("circuit breaker open")),
        "got {err:?}"
    );
}
