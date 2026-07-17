//! §4.1 — the one-time `url_verification` handshake returns its challenge for
//! the caller to echo back.

mod common;

use ardur_slack_adapter::{SlackEvent, SlackHeaders};

#[test]
fn url_verification_returns_challenge() {
    let adapter = common::test_adapter(None);
    let ts = common::NOW_UNIX.to_string();
    let body = serde_json::json!({
        "type": "url_verification",
        "token": "deprecated-verification-token",
        "challenge": "3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P"
    })
    .to_string();
    let headers = SlackHeaders::new(common::sign(&ts, &body), &ts);

    let event = adapter
        .parse_event_at(&headers, &body, common::NOW_UNIX)
        .expect("the handshake verifies and parses");

    assert_eq!(
        event,
        SlackEvent::UrlVerification {
            challenge: "3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P".to_string()
        }
    );
}
