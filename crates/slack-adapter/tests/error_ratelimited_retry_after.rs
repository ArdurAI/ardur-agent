//! §4.1 — an HTTP 429 send response maps to `SlackError::RateLimited` carrying
//! the parsed `Retry-After`, and lowers to `GatewayError::RateLimited`.

mod common;

use ardur_messaging_gateway::GatewayError;
use ardur_slack_adapter::SlackError;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn rate_limited_parses_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        // Slack signals rate limiting with HTTP 429 + a Retry-After (seconds).
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30"))
        .mount(&server)
        .await;

    let adapter = common::test_adapter(Some(server.uri()));
    let err = adapter
        .post_message("C123", "hi", None)
        .await
        .expect_err("a 429 is an error");

    assert!(
        matches!(err, SlackError::RateLimited { retry_after_ms } if retry_after_ms == 30_000),
        "got {err:?}"
    );

    // The trait boundary lowers RateLimited onto the fieldless GatewayError.
    let lowered = SlackError::RateLimited {
        retry_after_ms: 30_000,
    }
    .into_gateway_error("C123");
    assert!(
        matches!(lowered, GatewayError::RateLimited),
        "got {lowered:?}"
    );
}
