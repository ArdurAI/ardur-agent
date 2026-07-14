//! Scenario §4.4 — `email_channel`.
//!
//! Drives a full **fused turn** off an inbound **email** message, mirroring
//! the shape `ardur-channel-email`'s poll loop produces and the server's
//! worker routes:
//!
//! 1. An [`IncomingMessage`] is built exactly as the email adapter's
//!    `poll_once` builds one — an `email://<account>/<sender>` channel id, the
//!    sender's address, and a `Text` body (the parsed plaintext).
//! 2. Its text becomes a [`SubmitRequest`] and runs through the [`FusedRuntime`]
//!    (cap-token → cedar → cost-gate → provider → receipt), exactly as the
//!    server's `Processor::handle` does for any channel.
//! 3. The reply's target address is recovered from the namespaced channel id
//!    by the same `rsplit('/')` the server uses — proving the round-trip
//!    addresses the original sender, not the account's own address.
//!
//! The ungated test needs no network (the runtime's provider is the echo
//! stub). A second, **gated** test (`EMAIL_E2E=1` + the `ARDUR_EMAIL_*` env +
//! `EMAIL_TEST_RECIPIENT`) connects a *real* [`EmailChannel`], runs the same
//! fused turn, and sends the reply to a live inbox — the stub+live split the
//! E2E coverage rule asks of a channel feature.
//!
//! [`FusedRuntime`]: ardur_fused_runtime::FusedRuntime
//! [`EmailChannel`]: ardur_channel_email::EmailChannel

mod support;
use support::EchoProvider;

use std::sync::Arc;

use uuid::Uuid;

use ardur_e2e_tests::fixtures;

use ardur_channel_email::{EmailChannel, EmailConfig};
use ardur_fused_runtime::FusedRuntime;
use ardur_messaging_gateway::{
    ChannelId, IncomingMessage, MessageBody, MessageTarget, MessagingGateway, OutgoingMessage,
    SenderRef, UserRef,
};
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, SessionId, SubmitRequest};

/// The account address used to namespace channel ids in the ungated test.
const ACCOUNT: &str = "bot@example.com";
/// The sender the synthetic inbound message arrives from.
const SENDER: &str = "someone@example.com";

/// Build the [`IncomingMessage`] the email adapter's `poll_once` would emit
/// for a plaintext message from `sender`.
fn email_incoming(sender: &str, text: &str) -> IncomingMessage {
    IncomingMessage {
        message_id: Uuid::new_v4(),
        channel_id: ChannelId(format!("email://{ACCOUNT}/{sender}")),
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

/// The reply target the server recovers from a namespaced channel id — the
/// last `/`-segment (here, the sender's address).
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

/// Ungated: an email-shaped inbound message drives a full fused turn, and the
/// reply addresses the originating sender (not the account's own address).
#[tokio::test]
async fn email_inbound_message_drives_a_fused_turn() {
    const PROMPT: &str = "summarize this thread for the on-call handoff";

    let provider = Arc::new(EchoProvider::new());
    let runtime = runtime(provider.clone());

    let incoming = email_incoming(SENDER, PROMPT);

    let result = runtime
        .submit(user_request(&body_text(&incoming.body)))
        .await
        .expect("the email-origin turn completes through the fused substrate");

    assert_eq!(
        result.response.content, PROMPT,
        "the provider received the inbound email verbatim"
    );
    assert!(
        !result.receipt_id.0.is_nil(),
        "a real receipt id binds the completed turn"
    );

    assert_eq!(
        reply_target(&incoming.channel_id.0),
        SENDER,
        "the reply target is the originating sender, not the account's own address"
    );
}

/// Gated-live: connect a real email account, run the fused turn, and send the
/// reply to `EMAIL_TEST_RECIPIENT`. Skipped unless `EMAIL_E2E=1`.
#[tokio::test]
async fn email_live_fused_turn_round_trip() {
    if std::env::var("EMAIL_E2E").ok().as_deref() != Some("1") {
        eprintln!("skipping: set EMAIL_E2E=1 (plus ARDUR_EMAIL_* + EMAIL_TEST_RECIPIENT)");
        return;
    }
    let recipient = std::env::var("EMAIL_TEST_RECIPIENT")
        .expect("EMAIL_E2E=1 requires EMAIL_TEST_RECIPIENT (a deliverable address)");

    let config = EmailConfig::from_env().expect("ARDUR_EMAIL_* configures the account");
    let channel = EmailChannel::new(config)
        .await
        .expect("the account connects and validates its credentials");
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
            target: MessageTarget::User(UserRef(recipient)),
            body: MessageBody::Text(result.response.content.clone()),
            cap_token: CapTokenRef("e2e-cap".to_string()),
            parent_message_id: None,
        })
        .await
        .expect("the reply is accepted by SMTP");
    assert!(
        receipt.provider_message_id.is_some(),
        "the live reply carries a message id"
    );
}
