//! §4.1 — a message authored by this app's own bot is verified but dropped, so
//! the adapter never echoes itself into a loop.

mod common;

use ardur_slack_adapter::{SlackEvent, SlackHeaders};

#[test]
fn own_bot_message_is_filtered() {
    let adapter = common::test_adapter(None);
    let ts = common::NOW_UNIX.to_string();
    // `app_id` equal to the adapter's own app id marks this as our bot's post.
    let body = serde_json::json!({
        "type": "event_callback",
        "event": {
            "type": "message",
            "user": "UBOTUSER",
            "text": "this is my own bot talking",
            "channel": "C24680",
            "ts": "1700000000.000300",
            "bot_id": "B0SELFBOT",
            "app_id": common::APP_ID
        }
    })
    .to_string();
    let headers = SlackHeaders::new(common::sign(&ts, &body), &ts);

    let event = adapter
        .parse_event_at(&headers, &body, common::NOW_UNIX)
        .expect("the event verifies");

    assert_eq!(
        event,
        SlackEvent::Ignored,
        "the bot's own message is filtered, not surfaced as inbound"
    );
}
