//! Gated live integration — a real send against a running Telegram bot.
//!
//! This test is **skipped unless `TELEGRAM_INTEGRATION_TEST=1`** (so CI passes
//! without a live bot). When enabled, it reads the usual `TELEGRAM_*`
//! configuration (plus `TELEGRAM_TEST_CHAT` — a chat id the bot can post to),
//! connects, starts polling, posts a message into the chat, and asserts the send
//! returns a Telegram message id. It also checks that `receive` does not
//! immediately yield the bot's own echo within a short window (echo prevention).
//!
//! Provision a bot with @BotFather, start a chat with it (or add it to a group),
//! grab the chat id, then:
//!
//! ```bash
//! TELEGRAM_INTEGRATION_TEST=1 \
//! TELEGRAM_BOT_TOKEN=123456:ABC… \
//! TELEGRAM_TEST_CHAT=-1001234567890 \
//! cargo test -p ardur-channel-telegram --test integration -- --nocapture
//! ```

use std::time::Duration;

use ardur_channel_telegram::{TelegramChannel, TelegramConfig};
use ardur_messaging_gateway::{
    CapTokenRef, ChannelRef, MessageBody, MessageTarget, MessagingGateway, OutgoingMessage,
};
use uuid::Uuid;

/// Read a required var or return `None` so the test can self-skip.
fn var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

#[tokio::test]
async fn telegram_send_and_echo_filter() {
    if var("TELEGRAM_INTEGRATION_TEST").as_deref() != Some("1") {
        eprintln!(
            "skipping: set TELEGRAM_INTEGRATION_TEST=1 (plus TELEGRAM_* + TELEGRAM_TEST_CHAT)"
        );
        return;
    }
    let Some(chat_id) = var("TELEGRAM_TEST_CHAT") else {
        panic!("TELEGRAM_INTEGRATION_TEST=1 requires TELEGRAM_TEST_CHAT (a postable chat id)");
    };

    let config = TelegramConfig::from_env().expect("TELEGRAM_* env configures the bot");
    let channel = TelegramChannel::new(config)
        .await
        .expect("the bot connects and validates its token");
    channel.start();

    let body = format!("ardur integration ping {}", Uuid::new_v4());
    let receipt = channel
        .send_message(OutgoingMessage {
            message_id: Uuid::new_v4(),
            channel_id: channel.channel_id(),
            target: MessageTarget::Channel(ChannelRef(chat_id.clone())),
            body: MessageBody::Text(body.clone()),
            cap_token: CapTokenRef("integration-cap".to_string()),
            parent_message_id: None,
        })
        .await
        .expect("the message is accepted by Telegram");
    assert!(
        receipt.provider_message_id.is_some(),
        "the receipt carries the Telegram message id"
    );

    // The bot does not receive its own outgoing messages, and filters any that
    // would arrive; `receive` must not resolve with our message in the window.
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
