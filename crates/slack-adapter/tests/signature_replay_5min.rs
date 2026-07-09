//! §4.1 — a request older than the 5-minute replay window is rejected even when
//! its signature is otherwise valid.

mod common;

use ardur_slack_adapter::{SlackError, SlackEvent, SlackHeaders};

#[test]
fn stale_timestamp_is_rejected_as_replay() {
    let adapter = common::test_adapter(None);
    // Ten minutes in the past — double the ±5-minute window.
    let stale = common::NOW_UNIX - 600;
    let ts = stale.to_string();
    let body = serde_json::json!({ "type": "url_verification", "challenge": "abc123" }).to_string();
    // Sign correctly over the *stale* timestamp so the replay guard — not the
    // signature check — is what rejects it.
    let headers = SlackHeaders::new(common::sign(&ts, &body), &ts);

    let err = adapter
        .parse_event_at(&headers, &body, common::NOW_UNIX)
        .expect_err("a stale request is rejected as a replay");

    assert!(
        matches!(err, SlackError::Replay { age_seconds } if age_seconds == 600),
        "got {err:?}"
    );
}

#[test]
fn duplicate_signed_request_inside_window_is_rejected_as_replay() {
    let adapter = common::test_adapter(None);
    let ts = common::NOW_UNIX.to_string();
    let body = serde_json::json!({ "type": "url_verification", "challenge": "abc123" }).to_string();
    let headers = SlackHeaders::new(common::sign(&ts, &body), &ts);

    let first = adapter
        .parse_event_at(&headers, &body, common::NOW_UNIX)
        .expect("the first signed delivery is accepted");
    assert!(matches!(first, SlackEvent::UrlVerification { .. }));

    let err = adapter
        .parse_event_at(&headers, &body, common::NOW_UNIX)
        .expect_err("the exact same signed delivery is a replay even inside the timestamp window");
    assert!(
        matches!(err, SlackError::Replay { age_seconds } if age_seconds == 0),
        "got {err:?}"
    );
}
