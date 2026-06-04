//! Scenario §4.Y — `telegram_channel`.
//!
//! Drives a full **fused turn** off an inbound **Telegram** message, mirroring
//! the shape `ardur-channel-telegram`'s message endpoint produces and the
//! server's worker routes:
//!
//! 1. An [`IncomingMessage`] is built exactly as the Telegram adapter's
//!    `on_message` handler builds one — a `telegram://<bot>/<chat>` channel id,
//!    the sender's id, and a `Text` body.
//! 2. Its text becomes a [`SubmitRequest`] and runs through the [`FusedRuntime`]
//!    (cap-token → cedar → cost-gate → provider → receipt), exactly as the
//!    server's `Processor::handle` does for any channel.
//! 3. The reply's target chat is recovered from the namespaced channel id by the
//!    same `rsplit('/')` the server uses — proving the round-trip addresses the
//!    originating chat.
//!
//! The ungated test needs no network (the runtime's provider is the echo stub).
//! A second, **gated** test (`TELEGRAM_E2E=1` + the `TELEGRAM_*` env +
//! `TELEGRAM_TEST_CHAT`) connects a *real* [`TelegramChannel`], runs the same
//! fused turn, and posts the reply into a live chat — the stub+live split the
//! E2E coverage rule asks of a channel feature.
//!
//! [`FusedRuntime`]: ardur_fused_runtime::FusedRuntime
//! [`TelegramChannel`]: ardur_channel_telegram::TelegramChannel

mod support;
use support::EchoProvider;

use std::sync::Arc;

use uuid::Uuid;

use ardur_e2e_tests::fixtures;

use ardur_channel_telegram::{TelegramChannel, TelegramConfig};
use ardur_fused_runtime::FusedRuntime;
use ardur_messaging_gateway::{
    ChannelId, ChannelRef, IncomingMessage, MessageBody, MessageTarget, MessagingGateway,
    OutgoingMessage, SenderRef,
};
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, SessionId, SubmitRequest};

/// The bot's user id used to namespace channel ids in the ungated test.
const BOT_USER: &str = "7654321";
/// The chat the synthetic inbound message arrives in (a supergroup → negative).
const CHAT: &str = "-1001234567890";

/// Build the [`IncomingMessage`] the Telegram adapter's `on_message` handler
/// would emit for a text message in `chat` from `sender`.
fn telegram_incoming(chat: &str, sender: &str, text: &str) -> IncomingMessage {
    IncomingMessage {
        message_id: Uuid::new_v4(),
        channel_id: ChannelId(format!("telegram://{BOT_USER}/{chat}")),
        sender: SenderRef(sender.to_string()),
        body: MessageBody::Text(text.to_string()),
        received_at: 1_750_000_000_000,
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
/// `/`-segment (here, the chat id).
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

/// Ungated: a Telegram-shaped inbound message drives a full fused turn, and the
/// reply addresses the originating chat.
#[tokio::test]
async fn telegram_inbound_message_drives_a_fused_turn() {
    const PROMPT: &str = "draft a status update for the on-call channel";

    let provider = Arc::new(EchoProvider::new());
    let runtime = runtime(provider.clone());

    let incoming = telegram_incoming(CHAT, "424242", PROMPT);

    let result = runtime
        .submit(user_request(&body_text(&incoming.body)))
        .await
        .expect("the Telegram-origin turn completes through the fused substrate");

    assert_eq!(
        result.response.content, PROMPT,
        "the provider received the inbound Telegram message verbatim"
    );
    assert!(
        !result.receipt_id.0.is_nil(),
        "a real receipt id binds the completed turn"
    );

    assert_eq!(
        reply_target(&incoming.channel_id.0),
        CHAT,
        "the reply target is the originating Telegram chat"
    );
}

/// Gated-live: connect a real Telegram bot, run the fused turn, and post the
/// reply into `TELEGRAM_TEST_CHAT`. Skipped unless `TELEGRAM_E2E=1`.
#[tokio::test]
async fn telegram_live_fused_turn_round_trip() {
    if std::env::var("TELEGRAM_E2E").ok().as_deref() != Some("1") {
        eprintln!("skipping: set TELEGRAM_E2E=1 (plus TELEGRAM_* + TELEGRAM_TEST_CHAT)");
        return;
    }
    let chat_id = std::env::var("TELEGRAM_TEST_CHAT")
        .expect("TELEGRAM_E2E=1 requires TELEGRAM_TEST_CHAT (a postable chat id)");

    let config = TelegramConfig::from_env().expect("TELEGRAM_* configures the bot");
    let channel = TelegramChannel::new(config)
        .await
        .expect("the bot connects and validates its token");
    channel.start();

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
            target: MessageTarget::Channel(ChannelRef(chat_id)),
            body: MessageBody::Text(result.response.content.clone()),
            cap_token: CapTokenRef("e2e-cap".to_string()),
            parent_message_id: None,
        })
        .await
        .expect("the reply is accepted by Telegram");
    assert!(
        receipt.provider_message_id.is_some(),
        "the live reply carries the Telegram message id"
    );
}
