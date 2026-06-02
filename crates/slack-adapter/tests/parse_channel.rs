//! §4.1 — a signed channel message (with a thread) parses into an
//! `IncomingMessage` carrying its thread id.

mod common;

use ardur_messaging_gateway::{ChannelId, MessageBody, SenderRef, ThreadId};
use ardur_slack_adapter::{SlackEvent, SlackHeaders};

#[test]
fn signed_channel_message_parses_with_thread() {
    let adapter = common::test_adapter(None);
    let ts = common::NOW_UNIX.to_string();
    let body = serde_json::json!({
        "type": "event_callback",
        "event": {
            "type": "message",
            "user": "U99999",
            "text": "hello channel",
            // Slack public channels are prefixed `C`.
            "channel": "C24680",
            "ts": "1700000005.000000",
            "thread_ts": "1700000000.000100"
        }
    })
    .to_string();
    let headers = SlackHeaders::new(common::sign(&ts, &body), &ts);

    let event = adapter
        .parse_event_at(&headers, &body, common::NOW_UNIX)
        .expect("a signed channel message parses");

    let SlackEvent::Message(msg) = event else {
        panic!("expected a Message, got {event:?}");
    };
    assert_eq!(msg.sender, SenderRef("U99999".to_string()));
    assert_eq!(msg.body, MessageBody::Text("hello channel".to_string()));
    assert_eq!(
        msg.channel_id,
        ChannelId(format!("slack://{}/C24680", common::APP_ID))
    );
    assert_eq!(
        msg.thread_id,
        Some(ThreadId("1700000000.000100".to_string())),
        "the thread_ts is carried onto the incoming message"
    );
}
