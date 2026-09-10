//! Integration test for the outbound webhook delivery client: drives
//! `OutboundWebhookClient::send` against a real mocked HTTP endpoint,
//! exercising signing, success, permanent-failure, and retry-then-succeed
//! paths — none of which had any coverage before.

use std::time::Duration;

use ardur_webhook::{EventType, OutboundWebhookClient, OutboundWebhookConfig, WebhookEvent};

use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn send_succeeds_and_signs_the_body_on_a_2xx_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hooks/deploy"))
        .and(header_exists("x-webhook-signature"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let config = OutboundWebhookConfig::new(format!("{}/hooks/deploy", server.uri()), "out-secret");
    let client = OutboundWebhookClient::new(config).expect("client builds");
    let event = WebhookEvent::new(
        EventType::Deploy,
        "ardur",
        serde_json::json!({"env": "prod"}),
    );

    client.send(&event).await.expect("delivery succeeds");
}

#[tokio::test]
async fn send_retries_transient_5xx_then_succeeds() {
    let server = MockServer::start().await;
    // First attempt fails with 503, second succeeds — proves the retry loop
    // actually re-sends rather than giving up on the first transient error.
    Mock::given(method("POST"))
        .and(path("/hooks/flaky"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/hooks/flaky"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let config = OutboundWebhookConfig::new(format!("{}/hooks/flaky", server.uri()), "out-secret")
        .with_max_retries(2);
    let client = OutboundWebhookClient::new(config).expect("client builds");
    let event = WebhookEvent::new(EventType::Build, "ardur", serde_json::json!({"ok": true}));

    client
        .send(&event)
        .await
        .expect("delivery succeeds after retry");

    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(requests.len(), 2, "one failed attempt plus one retry");
}

#[tokio::test]
async fn send_fails_fast_on_a_permanent_4xx_without_retrying() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hooks/rejected"))
        .respond_with(ResponseTemplate::new(400))
        .expect(1)
        .mount(&server)
        .await;

    let config =
        OutboundWebhookConfig::new(format!("{}/hooks/rejected", server.uri()), "out-secret")
            .with_max_retries(3);
    let client = OutboundWebhookClient::new(config).expect("client builds");
    let event = WebhookEvent::new(EventType::Issue, "ardur", serde_json::json!({}));

    let err = client.send(&event).await.expect_err("400 is not retried");
    assert!(
        err.to_string().contains("400"),
        "error should reference the permanent status code: {err}"
    );

    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        requests.len(),
        1,
        "a permanent 4xx must not trigger a retry"
    );
}

#[tokio::test]
async fn send_exhausts_retries_and_reports_failure_on_sustained_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hooks/down"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let config = OutboundWebhookConfig::new(format!("{}/hooks/down", server.uri()), "out-secret")
        .with_max_retries(2)
        .with_timeout(Duration::from_secs(5));
    let client = OutboundWebhookClient::new(config).expect("client builds");
    let event = WebhookEvent::new(EventType::PullRequest, "ardur", serde_json::json!({}));

    let err = client
        .send(&event)
        .await
        .expect_err("sustained 5xx exhausts retries");
    assert!(err.to_string().contains("500"));

    let requests = server.received_requests().await.expect("requests recorded");
    assert_eq!(
        requests.len(),
        3,
        "the initial attempt plus 2 configured retries"
    );
}

#[tokio::test]
async fn build_request_produces_a_verifiable_signature_without_sending() {
    let config = OutboundWebhookConfig::new("http://example.invalid/hooks", "out-secret");
    let client = OutboundWebhookClient::new(config.clone()).expect("client builds");
    let event = WebhookEvent::new(EventType::Comment, "ardur", serde_json::json!({"n": 1}));

    let (body, headers) = client
        .build_request(&event)
        .expect("builds a signed request");
    let signature = headers
        .iter()
        .find(|(name, _)| name == &config.signature_header)
        .map(|(_, value)| value.clone())
        .expect("signature header present");

    assert!(ardur_webhook::verify_signature(&body, &config.secret, &signature).is_ok());
}
