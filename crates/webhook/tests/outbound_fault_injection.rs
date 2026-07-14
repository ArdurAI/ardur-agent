//! Fault-injection tests for the resilience layer wired into
//! `OutboundWebhookClient` (`ardur-resilience`).

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use ardur_resilience::circuit_breaker::CircuitBreakerConfig;
use ardur_webhook::{EventType, OutboundWebhookClient, OutboundWebhookConfig, WebhookEvent};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn event() -> WebhookEvent {
    WebhookEvent::new(EventType::Push, "test", serde_json::json!({"ok": true}))
}

#[tokio::test]
async fn transient_500_is_retried_into_success() {
    let server = MockServer::start().await;
    let calls = std::sync::Arc::new(AtomicU32::new(0));
    let calls_clone = calls.clone();

    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            let n = calls_clone.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                ResponseTemplate::new(500)
            } else {
                ResponseTemplate::new(200)
            }
        })
        .mount(&server)
        .await;

    let client = OutboundWebhookClient::new(
        OutboundWebhookConfig::new(server.uri(), "secret").with_max_retries(3),
    )
    .expect("client builds");

    client.send(&event()).await.expect("third attempt succeeds");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn permanent_400_is_never_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400))
        .expect(1)
        .mount(&server)
        .await;

    let client = OutboundWebhookClient::new(
        OutboundWebhookConfig::new(server.uri(), "secret").with_max_retries(3),
    )
    .expect("client builds");

    let err = client.send(&event()).await.expect_err("400 is fatal");
    assert!(format!("{err}").contains("400"));
}

#[tokio::test]
async fn open_breaker_fails_fast_without_any_further_network_calls() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(2)
        .mount(&server)
        .await;

    let client = OutboundWebhookClient::new(
        OutboundWebhookConfig::new(server.uri(), "secret")
            .with_max_retries(0)
            .with_circuit_breaker(CircuitBreakerConfig {
                failure_threshold: 2,
                open_duration: Duration::from_secs(60),
            }),
    )
    .expect("client builds");

    for _ in 0..2 {
        client.send(&event()).await.expect_err("500 is an error");
    }

    let err = client.send(&event()).await.expect_err("breaker is open");
    assert!(
        format!("{err}").contains("circuit breaker open"),
        "got {err}"
    );
}
