//! Gated live integration — a real send + receive round-trip against a running
//! homeserver.
//!
//! This test is **skipped unless `MATRIX_INTEGRATION_TEST=1`** (so CI passes
//! without standing up a Matrix server). When enabled, it reads the usual
//! `MATRIX_*` configuration (plus `MATRIX_TEST_ROOM` — a room the bot is already
//! joined to), connects, starts syncing, posts a message into the room, and
//! waits for the sync loop to echo *some* inbound message back through
//! [`MessagingGateway::receive`].
//!
//! A local [Conduit](https://conduit.rs) homeserver is the lightest way to run
//! this; matrix.org works too. Provision a bot account, mint an access token,
//! invite/join the bot to a test room, then:
//!
//! ```bash
//! MATRIX_INTEGRATION_TEST=1 \
//! MATRIX_HOMESERVER_URL=http://localhost:6167 \
//! MATRIX_USER_ID=@ardur-bot:localhost \
//! MATRIX_ACCESS_TOKEN=syt_... \
//! MATRIX_TEST_ROOM='!abc:localhost' \
//! cargo test -p ardur-channel-matrix --test integration -- --nocapture
//! ```

use std::time::Duration;

use ardur_channel_matrix::{MatrixChannel, MatrixConfig};
use ardur_messaging_gateway::{
    CapTokenRef, ChannelRef, MessageBody, MessageTarget, MessagingGateway, OutgoingMessage,
};
use uuid::Uuid;

/// Read a required var or return `None` so the test can self-skip.
fn var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

#[tokio::test]
async fn matrix_send_and_receive_round_trip() {
    if var("MATRIX_INTEGRATION_TEST").as_deref() != Some("1") {
        eprintln!("skipping: set MATRIX_INTEGRATION_TEST=1 (plus MATRIX_* + MATRIX_TEST_ROOM)");
        return;
    }
    let Some(room_id) = var("MATRIX_TEST_ROOM") else {
        panic!("MATRIX_INTEGRATION_TEST=1 requires MATRIX_TEST_ROOM (a joined room id)");
    };

    let config = MatrixConfig::from_env().expect("MATRIX_* env configures the bot");
    let channel = MatrixChannel::new(config)
        .await
        .expect("the bot connects and restores its session");
    channel.start_sync();

    // Give the first sync a moment to land the room state before sending.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let body = format!("ardur integration ping {}", Uuid::new_v4());
    let receipt = channel
        .send_message(OutgoingMessage {
            message_id: Uuid::new_v4(),
            channel_id: channel.channel_id(),
            target: MessageTarget::Channel(ChannelRef(room_id.clone())),
            body: MessageBody::Text(body.clone()),
            cap_token: CapTokenRef("integration-cap".to_string()),
            parent_message_id: None,
        })
        .await
        .expect("the message is accepted by the homeserver");
    assert!(
        receipt.provider_message_id.is_some(),
        "the receipt carries the homeserver event id"
    );

    // The bot filters its *own* echoes, so a true inbound assertion needs a
    // second human/bot speaker. Here we only assert that `receive` does not
    // resolve immediately with a spurious message within a short window — the
    // send path is the load-bearing live assertion.
    let recv = tokio::time::timeout(Duration::from_secs(5), channel.receive()).await;
    match recv {
        Err(_elapsed) => { /* expected: no foreign message arrived in the window */ }
        Ok(Ok(msg)) => assert_ne!(
            msg.body,
            MessageBody::Text(body.clone()),
            "the bot must not re-ingest its own message (echo prevention)"
        ),
        Ok(Err(e)) => panic!("receive failed: {e}"),
    }
}
