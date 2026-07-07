//! ARD-475 — the Slack sender allowlist is deny-by-default.

mod common;

use ardur_slack_adapter::{SlackAdapter, SlackEvent, SlackHeaders};
use secrecy::SecretString;

/// Sign and parse a `message` event from `user`, returning the parsed outcome.
fn signed_message(adapter: &SlackAdapter, user: &str) -> SlackEvent {
    let ts = common::NOW_UNIX.to_string();
    let body = serde_json::json!({
        "type": "event_callback",
        "event": {
            "type": "message",
            "user": user,
            "text": "hi",
            "channel": "C1",
            "ts": "1700000000.000000"
        }
    })
    .to_string();
    let headers = SlackHeaders::new(common::sign(&ts, &body), &ts);
    adapter
        .parse_event_at(&headers, &body, common::NOW_UNIX)
        .expect("a signed event parses")
}

fn bare_adapter() -> SlackAdapter {
    SlackAdapter::new(
        SecretString::from(common::BOT_TOKEN.to_string()),
        SecretString::from(common::SIGNING_SECRET.to_string()),
        common::APP_ID.to_string(),
    )
}

#[test]
fn empty_allowlist_denies_every_sender() {
    // ARD-475: no allowlist configured -> deny-by-default (every sender dropped).
    let adapter = bare_adapter();
    assert_eq!(signed_message(&adapter, "U12345"), SlackEvent::Ignored);
    assert_eq!(signed_message(&adapter, "U77777"), SlackEvent::Ignored);
}

#[test]
fn allowlisted_sender_passes_others_dropped() {
    let adapter = bare_adapter().with_allowed_senders(["U12345"]);
    assert!(
        matches!(signed_message(&adapter, "U12345"), SlackEvent::Message(_)),
        "an allowlisted sender's message is surfaced"
    );
    assert_eq!(
        signed_message(&adapter, "U77777"),
        SlackEvent::Ignored,
        "a non-allowlisted sender is dropped"
    );
}
