//! Gated live integration — a real send against a running Discord bot.
//!
//! This test is **skipped unless `DISCORD_INTEGRATION_TEST=1`** (so CI passes
//! without a live bot). When enabled, it reads the usual `DISCORD_*`
//! configuration (plus `DISCORD_TEST_CHANNEL` — a channel id the bot can post
//! to), builds the channel, starts the gateway, posts a message into the
//! channel, and asserts the send returns a Discord message id. It also checks
//! that `receive` does not immediately yield the bot's own echo within a short
//! window (echo prevention).
//!
//! Provision a bot in the Discord developer portal, enable the privileged
//! **Message Content** intent, invite it to a server, then:
//!
//! ```bash
//! DISCORD_INTEGRATION_TEST=1 \
//! DISCORD_BOT_TOKEN=… \
//! DISCORD_APPLICATION_ID=… \
//! DISCORD_TEST_CHANNEL=123456789012345678 \
//! cargo test -p ardur-channel-discord --test integration -- --nocapture
//! ```

use std::time::Duration;

use ardur_channel_discord::{DiscordChannel, DiscordConfig};
use ardur_messaging_gateway::{
    CapTokenRef, ChannelRef, MessageBody, MessageTarget, MessagingGateway, OutgoingMessage,
};
use uuid::Uuid;

/// Read a required var or return `None` so the test can self-skip.
fn var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

#[tokio::test]
async fn discord_send_and_echo_filter() {
    if var("DISCORD_INTEGRATION_TEST").as_deref() != Some("1") {
        eprintln!(
            "skipping: set DISCORD_INTEGRATION_TEST=1 (plus DISCORD_* + DISCORD_TEST_CHANNEL)"
        );
        return;
    }
    let Some(channel_id) = var("DISCORD_TEST_CHANNEL") else {
        panic!("DISCORD_INTEGRATION_TEST=1 requires DISCORD_TEST_CHANNEL (a postable channel id)");
    };

    let config = DiscordConfig::from_env().expect("DISCORD_* env configures the bot");
    let channel = DiscordChannel::new(config)
        .await
        .expect("the serenity client builds");
    channel.start().await;

    // Give the gateway a moment to connect before sending.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let body = format!("ardur integration ping {}", Uuid::new_v4());
    let receipt = channel
        .send_message(OutgoingMessage {
            message_id: Uuid::new_v4(),
            channel_id: channel.channel_id(),
            target: MessageTarget::Channel(ChannelRef(channel_id.clone())),
            body: MessageBody::Text(body.clone()),
            cap_token: CapTokenRef("integration-cap".to_string()),
            parent_message_id: None,
        })
        .await
        .expect("the message is accepted by Discord");
    assert!(
        receipt.provider_message_id.is_some(),
        "the receipt carries the Discord message id"
    );

    // The bot filters its *own* echoes, so `receive` must not resolve with the
    // message we just posted within a short window.
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
