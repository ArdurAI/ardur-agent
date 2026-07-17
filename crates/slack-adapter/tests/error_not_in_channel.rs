//! §4.1 — a `not_in_channel` send rejection maps to `SlackError::Forbidden`,
//! which lowers to `GatewayError::DeliveryFailed` at the trait boundary.

mod common;

use ardur_messaging_gateway::GatewayError;
use ardur_slack_adapter::SlackError;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn not_in_channel_maps_to_forbidden() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({ "ok": false, "error": "not_in_channel" })),
        )
        .mount(&server)
        .await;

    let adapter = common::test_adapter(Some(server.uri()));
    let err = adapter
        .post_message("C404", "hi", None)
        .await
        .expect_err("not_in_channel is an error");

    assert!(matches!(err, SlackError::Forbidden), "got {err:?}");

    // The trait boundary lowers Forbidden onto DeliveryFailed (carrying the
    // Display text so the distinction survives).
    let lowered = SlackError::Forbidden.into_gateway_error("C404");
    assert!(
        matches!(lowered, GatewayError::DeliveryFailed(_)),
        "got {lowered:?}"
    );
}
