//! §4.1 — a request whose signature does not match the body is rejected, even
//! when the timestamp is fresh.

mod common;

use ardur_slack_adapter::{SlackError, SlackHeaders};

#[test]
fn mismatched_signature_is_rejected() {
    let adapter = common::test_adapter(None);
    let ts = common::NOW_UNIX.to_string();
    let body = serde_json::json!({ "type": "url_verification", "challenge": "abc123" }).to_string();
    // A well-formed-but-wrong signature (correct shape, never computed over the
    // body) must not slip through the constant-time compare.
    let headers = SlackHeaders::new(format!("v0={}", "0".repeat(64)), &ts);

    let err = adapter
        .parse_event_at(&headers, &body, common::NOW_UNIX)
        .expect_err("a mismatched signature is rejected");

    assert!(matches!(err, SlackError::InvalidSignature), "got {err:?}");
}
