//! §4.1 — a signed direct-message event parses into an `IncomingMessage`.

mod common;

use ardur_messaging_gateway::{ChannelId, MessageBody, SenderRef};
use ardur_slack_adapter::{SlackEvent, SlackHeaders};

#[test]
fn signed_dm_parses_into_incoming_message() {
    let adapter = common::test_adapter(None);
    let ts = common::NOW_UNIX.to_string();
    let body = serde_json::json!({
        "type": "event_callback",
        "event": {
            "type": "message",
            "user": "U12345",
            "text": "hello from a dm",
            // Slack DM channels are prefixed `D`.
            "channel": "D67890",
            "ts": "1700000000.200000"
        }
    })
    .to_string();
    let headers = SlackHeaders::new(common::sign(&ts, &body), &ts);

    let event = adapter
        .parse_event_at(&headers, &body, common::NOW_UNIX)
        .expect("a signed dm parses");

    let SlackEvent::Message(msg) = event else {
        panic!("expected a Message, got {event:?}");
    };
    assert_eq!(msg.sender, SenderRef("U12345".to_string()));
    assert_eq!(msg.body, MessageBody::Text("hello from a dm".to_string()));
    assert_eq!(
        msg.channel_id,
        ChannelId(format!("slack://{}/D67890", common::APP_ID))
    );
    // ts "1700000000.200000" (200_000 µs = 200 ms) → 1_700_000_000_200 ms.
    assert_eq!(msg.received_at, 1_700_000_000_200);
    assert!(msg.thread_id.is_none(), "a top-level dm has no thread");
}
