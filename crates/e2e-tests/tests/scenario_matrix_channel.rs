//! Scenario §4.X — `matrix_channel`.
//!
//! Drives a full **fused turn** off an inbound **Matrix** message, mirroring the
//! shape `ardur-channel-matrix`'s sync handler produces and the server's worker
//! routes:
//!
//! 1. An [`IncomingMessage`] is built exactly as the Matrix adapter's
//!    `on_room_message` handler builds one — a `matrix://<bot>/<room>` channel
//!    id, the sender's Matrix id, and a `Text` body.
//! 2. Its text becomes a [`SubmitRequest`] and runs through the [`FusedRuntime`]
//!    (cap-token → cedar → cost-gate → provider → receipt), exactly as the
//!    server's `Processor::handle` does for a Slack message.
//! 3. The reply's target room is recovered from the namespaced channel id by the
//!    same `rsplit('/')` the server uses — proving the round-trip addresses the
//!    originating room.
//!
//! The ungated test needs no network (the runtime's provider is the echo stub).
//! A second, **gated** test (`MATRIX_E2E=1` + the `MATRIX_*` env +
//! `MATRIX_TEST_ROOM`) connects a *real* [`MatrixChannel`], runs the same fused
//! turn, and posts the reply into a live room — the provider-style stub+live
//! split the E2E coverage rule asks of a channel feature.
//!
//! [`FusedRuntime`]: ardur_fused_runtime::FusedRuntime
//! [`MatrixChannel`]: ardur_channel_matrix::MatrixChannel

mod support;
use support::EchoProvider;

use std::sync::Arc;

use uuid::Uuid;

use ardur_e2e_tests::fixtures;

use ardur_channel_matrix::{MatrixChannel, MatrixConfig};
use ardur_fused_runtime::FusedRuntime;
use ardur_messaging_gateway::{
    ChannelId, ChannelRef, IncomingMessage, MessageBody, MessageTarget, MessagingGateway,
    OutgoingMessage, SenderRef, UnixTsMillis,
};
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, SessionId, SubmitRequest};

/// The bot's Matrix user id used to namespace channel ids in the ungated test.
const BOT_USER: &str = "@ardur-bot:example.org";
/// The room the synthetic inbound message arrives in.
const ROOM: &str = "!testroom:example.org";

/// Build the [`IncomingMessage`] the Matrix adapter's `on_room_message` handler
/// would emit for a text event in `room` from `sender`.
fn matrix_incoming(room: &str, sender: &str, text: &str) -> IncomingMessage {
    IncomingMessage {
        message_id: Uuid::new_v4(),
        channel_id: ChannelId(format!("matrix://{BOT_USER}/{room}")),
        sender: SenderRef(sender.to_string()),
        body: MessageBody::Text(text.to_string()),
        received_at: UnixTsMillis(1_750_000_000_000),
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
/// `/`-segment (here, the room id).
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

/// Ungated: a Matrix-shaped inbound message drives a full fused turn, and the
/// reply addresses the originating room.
#[tokio::test]
async fn matrix_inbound_message_drives_a_fused_turn() {
    const PROMPT: &str = "summarize the deployment runbook";

    let provider = Arc::new(EchoProvider::new());
    let runtime = runtime(provider.clone());

    let incoming = matrix_incoming(ROOM, "@human:example.org", PROMPT);

    // The fused turn runs exactly as the server's worker runs it for any channel.
    let result = runtime
        .submit(user_request(&body_text(&incoming.body)))
        .await
        .expect("the Matrix-origin turn completes through the fused substrate");

    // The echo provider returns the last user message, proving the original text
    // reached the provider intact through the runtime.
    assert_eq!(
        result.response.content, PROMPT,
        "the provider received the inbound Matrix message verbatim"
    );
    // The turn minted a receipt — the substrate ran end to end.
    assert!(
        !result.receipt_id.0.is_nil(),
        "a real receipt id binds the completed turn"
    );

    // The reply routes back to the originating room (the server's `is_matrix`
    // branch posts here via `MatrixChannel::send_text`).
    assert_eq!(
        reply_target(&incoming.channel_id.0),
        ROOM,
        "the reply target is the originating Matrix room"
    );
}

/// Gated-live: connect a real Matrix bot, run the fused turn, and post the reply
/// into `MATRIX_TEST_ROOM`. Skipped unless `MATRIX_E2E=1`.
#[tokio::test]
async fn matrix_live_fused_turn_round_trip() {
    if std::env::var("MATRIX_E2E").ok().as_deref() != Some("1") {
        eprintln!("skipping: set MATRIX_E2E=1 (plus MATRIX_* + MATRIX_TEST_ROOM)");
        return;
    }
    let room = std::env::var("MATRIX_TEST_ROOM")
        .expect("MATRIX_E2E=1 requires MATRIX_TEST_ROOM (a joined room id)");

    // 1. Connect the real channel.
    let config = MatrixConfig::from_env().expect("MATRIX_* configures the bot");
    let channel = MatrixChannel::new(config)
        .await
        .expect("the bot connects and restores its session");
    channel.start_sync();

    // 2. Run a fused turn for a synthesized inbound message.
    let provider = Arc::new(EchoProvider::new());
    let runtime = runtime(provider);
    let prompt = format!("ardur live e2e {}", Uuid::new_v4());
    let result = runtime
        .submit(user_request(&prompt))
        .await
        .expect("the fused turn completes");

    // 3. Post the reply back into the live room through the gateway trait.
    let receipt = channel
        .send_message(OutgoingMessage {
            message_id: Uuid::new_v4(),
            channel_id: channel.channel_id(),
            target: MessageTarget::Channel(ChannelRef(room)),
            body: MessageBody::Text(result.response.content.clone()),
            cap_token: CapTokenRef("e2e-cap".to_string()),
            parent_message_id: None,
        })
        .await
        .expect("the reply is accepted by the homeserver");
    assert!(
        receipt.provider_message_id.is_some(),
        "the live reply carries the homeserver event id"
    );
}
