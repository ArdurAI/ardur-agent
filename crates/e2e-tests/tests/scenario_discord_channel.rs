//! Scenario §4.Y — `discord_channel`.
//!
//! Drives a full **fused turn** off an inbound **Discord** message, mirroring
//! the shape `ardur-channel-discord`'s `message` handler produces and the
//! server's worker routes:
//!
//! 1. An [`IncomingMessage`] is built exactly as the Discord adapter's
//!    `on_message` handler builds one — a `discord://<app>/<channel>` channel
//!    id, the author's id as the sender, and a `Text` body.
//! 2. Its text becomes a [`SubmitRequest`] and runs through the [`FusedRuntime`]
//!    (cap-token → cedar → cost-gate → provider → receipt), exactly as the
//!    server's `Processor::handle` does for any channel.
//! 3. The reply's target channel is recovered from the namespaced channel id by
//!    the same `rsplit('/')` the server uses — proving the round-trip addresses
//!    the originating channel.
//!
//! The ungated test needs no network (the runtime's provider is the echo stub).
//! A second, **gated** test (`DISCORD_E2E=1` + the `DISCORD_*` env +
//! `DISCORD_TEST_CHANNEL`) connects a *real* [`DiscordChannel`], runs the same
//! fused turn, and posts the reply into a live channel — the stub+live split the
//! E2E coverage rule asks of a channel feature.
//!
//! [`FusedRuntime`]: ardur_fused_runtime::FusedRuntime
//! [`DiscordChannel`]: ardur_channel_discord::DiscordChannel

mod support;
use support::EchoProvider;

use std::sync::Arc;

use uuid::Uuid;

use ardur_e2e_tests::fixtures;

use ardur_channel_discord::{DiscordChannel, DiscordConfig};
use ardur_fused_runtime::FusedRuntime;
use ardur_messaging_gateway::{
    ChannelId, ChannelRef, IncomingMessage, MessageBody, MessageTarget, MessagingGateway,
    OutgoingMessage, SenderRef,
};
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, SessionId, SubmitRequest};

/// The bot's application id used to namespace channel ids in the ungated test.
const BOT_APP: &str = "123456789012345678";
/// The channel the synthetic inbound message arrives in.
const CHANNEL: &str = "987654321098765432";

/// Build the [`IncomingMessage`] the Discord adapter's `on_message` handler
/// would emit for a text message in `channel` from `sender`.
fn discord_incoming(channel: &str, sender: &str, text: &str) -> IncomingMessage {
    IncomingMessage {
        message_id: Uuid::new_v4(),
        channel_id: ChannelId(format!("discord://{BOT_APP}/{channel}")),
        sender: SenderRef(sender.to_string()),
        body: MessageBody::Text(text.to_string()),
        received_at: ardur_messaging_gateway::UnixTsMillis(1_750_000_000_000),
        thread_id: None,
    }
}

/// The text the worker hands to the runtime, pulled from an inbound body.
fn body_text(body: &MessageBody) -> String {
    match body {
        MessageBody::Text(t) | MessageBody::Markdown(t) => t.clone(),
        MessageBody::Mention { body, .. } => body.clone(),
        MessageBody::Attachment { .. } => String::new(),
    }
}

/// The reply target the server recovers from a namespaced channel id — the last
/// `/`-segment (here, the channel id).
fn reply_target(channel_id: &str) -> &str {
    channel_id.rsplit('/').next().unwrap_or(channel_id)
}

fn user_request(text: &str) -> SubmitRequest {
    SubmitRequest {
        messages: vec![ChatMessage::user(text)],
        cap_token: CapTokenRef(fixtures::dev_valid_cap_token()),
        session_id: SessionId::new(),
        requested_provider: None,
    }
}

fn runtime(provider: Arc<EchoProvider>) -> FusedRuntime {
    fixtures::fused_builder(provider)
        .build()
        .expect("the fused runtime wires")
}

/// Ungated: a Discord-shaped inbound message drives a full fused turn, and the
/// reply addresses the originating channel.
#[tokio::test]
async fn discord_inbound_message_drives_a_fused_turn() {
    const PROMPT: &str = "summarize the incident timeline";

    let provider = Arc::new(EchoProvider::new());
    let runtime = runtime(provider.clone());

    let incoming = discord_incoming(CHANNEL, "555000111222333444", PROMPT);

    let result = runtime
        .submit(user_request(&body_text(&incoming.body)))
        .await
        .expect("the Discord-origin turn completes through the fused substrate");

    assert_eq!(
        result.response.content, PROMPT,
        "the provider received the inbound Discord message verbatim"
    );
    assert!(
        !result.receipt_id.0.is_nil(),
        "a real receipt id binds the completed turn"
    );

    assert_eq!(
        reply_target(&incoming.channel_id.0),
        CHANNEL,
        "the reply target is the originating Discord channel"
    );
}

/// Gated-live: connect a real Discord bot, run the fused turn, and post the
/// reply into `DISCORD_TEST_CHANNEL`. Skipped unless `DISCORD_E2E=1`.
#[tokio::test]
async fn discord_live_fused_turn_round_trip() {
    if std::env::var("DISCORD_E2E").ok().as_deref() != Some("1") {
        eprintln!("skipping: set DISCORD_E2E=1 (plus DISCORD_* + DISCORD_TEST_CHANNEL)");
        return;
    }
    let channel_id = std::env::var("DISCORD_TEST_CHANNEL")
        .expect("DISCORD_E2E=1 requires DISCORD_TEST_CHANNEL (a postable channel id)");

    let config = DiscordConfig::from_env().expect("DISCORD_* configures the bot");
    let channel = DiscordChannel::new(config)
        .await
        .expect("the serenity client builds");
    channel.start().await;

    let provider = Arc::new(EchoProvider::new());
    let runtime = runtime(provider);
    let prompt = format!("ardur live e2e {}", Uuid::new_v4());
    let result = runtime
        .submit(user_request(&prompt))
        .await
        .expect("the fused turn completes");

    let receipt = channel
        .send_message(OutgoingMessage {
            message_id: Uuid::new_v4(),
            channel_id: channel.channel_id(),
            target: MessageTarget::Channel(ChannelRef(channel_id)),
            body: MessageBody::Text(result.response.content.clone()),
            cap_token: CapTokenRef("e2e-cap".to_string()),
            parent_message_id: None,
        })
        .await
        .expect("the reply is accepted by Discord");
    assert!(
        receipt.provider_message_id.is_some(),
        "the live reply carries the Discord message id"
    );
}
