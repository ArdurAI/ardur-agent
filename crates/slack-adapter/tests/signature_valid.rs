//! §4.1 — a correctly-signed, fresh request passes HMAC verification.

mod common;

use ardur_slack_adapter::{SlackEvent, SlackHeaders};

#[test]
fn correctly_signed_fresh_request_verifies() {
    let adapter = common::test_adapter(None);
    let ts = common::NOW_UNIX.to_string();
    let body = serde_json::json!({ "type": "url_verification", "challenge": "abc123" }).to_string();
    let headers = SlackHeaders::new(common::sign(&ts, &body), &ts);

    let event = adapter
        .parse_event_at(&headers, &body, common::NOW_UNIX)
        .expect("a correctly-signed fresh request verifies and parses");

    assert_eq!(
        event,
        SlackEvent::UrlVerification {
            challenge: "abc123".to_string()
        }
    );
}
