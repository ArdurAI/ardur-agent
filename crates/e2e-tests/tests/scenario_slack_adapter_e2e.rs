//! Scenario §4.1 — `slack_adapter_e2e`.
//!
//! Drives the Slack adapter end-to-end across *both* directions of the §4.0
//! [`MessagingGateway`] contract, against a mocked Slack:
//!
//! 1. **Outbound** — a real [`OutgoingMessage`] flows through
//!    [`MessagingGateway::send_message`] (the adapter behind `dyn`), POSTing to a
//!    wiremock `chat.postMessage`. The mock proves it observed the `Bearer`
//!    bot-token auth header and the `{channel,text}` JSON body, and the returned
//!    [`MessageReceipt`] carries Slack's `ts` as the `provider_message_id`.
//! 2. **Inbound** — a genuinely HMAC-signed Slack `message` event is synthesized
//!    and fed to [`SlackAdapter::parse_event_at`], which verifies the signature
//!    and lifts it into an [`IncomingMessage`].
//!
//! Unlike the per-crate suite in `crates/slack-adapter/tests`, this scenario
//! lives in the cross-crate host and exercises the adapter through the gateway
//! trait object — the shape the runtime will hold it as.
//!
//! [`MessagingGateway`]: ardur_messaging_gateway::MessagingGateway
//! [`MessagingGateway::send_message`]: ardur_messaging_gateway::MessagingGateway::send_message

use hmac::{Hmac, KeyInit, Mac};
use secrecy::SecretString;
use sha2::Sha256;
use uuid::Uuid;

use ardur_messaging_gateway::{
    CapTokenRef, ChannelId, ChannelRef, MessageBody, MessageTarget, MessagingGateway,
    OutgoingMessage, SenderRef,
};
use ardur_slack_adapter::{SlackAdapter, SlackEvent, SlackHeaders};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

type HmacSha256 = Hmac<Sha256>;

const BOT_TOKEN: &str = "xoxb-e2e-test-token";
const SIGNING_SECRET: &str = "e2e-signing-secret-0000000000abcd";
const APP_ID: &str = "A0E2ESLACK";
/// A fixed "now" (Unix seconds) for the deterministic replay check.
const NOW_UNIX: u64 = 1_750_000_000;

/// Recompute the genuine Slack `v0=<hex>` request signature.
fn sign(timestamp: &str, body: &str) -> String {
    let basestring = format!("v0:{timestamp}:{body}");
    let mut mac =
        HmacSha256::new_from_slice(SIGNING_SECRET.as_bytes()).expect("hmac accepts any key length");
    mac.update(basestring.as_bytes());
    format!("v0={}", hex::encode(mac.finalize().into_bytes()))
}

#[tokio::test]
async fn slack_adapter_sends_and_parses_end_to_end() {
    // ---- 1. Outbound: send through the gateway trait to a mocked Slack.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "ts": "1700000000.000100",
            "channel": "C12345"
        })))
        .mount(&server)
        .await;

    let adapter = SlackAdapter::new(
        SecretString::from(BOT_TOKEN.to_string()),
        SecretString::from(SIGNING_SECRET.to_string()),
        APP_ID.to_string(),
    )
    .with_base_url(server.uri());

    // Hold it as the trait object the runtime would wire into the registry.
    let gateway: &dyn MessagingGateway = &adapter;
    assert_eq!(gateway.channel_id(), ChannelId(format!("slack://{APP_ID}")));
    assert!(!gateway.supports_threading(), "threading is Phase 2");

    let message_id = Uuid::new_v4();
    let receipt = gateway
        .send_message(OutgoingMessage {
            message_id,
            channel_id: gateway.channel_id(),
            target: MessageTarget::Channel(ChannelRef("C12345".to_string())),
            body: MessageBody::Text("hello from ardur".to_string()),
            cap_token: CapTokenRef("e2e-cap".to_string()),
            parent_message_id: None,
        })
        .await
        .expect("the send is accepted by the mocked chat.postMessage");

    // Slack's `ts` is surfaced as the provider message id; the receipt id reuses
    // the caller's correlation id so the delivery binds to its run.
    assert_eq!(
        receipt.provider_message_id.as_deref(),
        Some("1700000000.000100")
    );
    assert_eq!(receipt.receipt_id, message_id);
    assert_eq!(receipt.delivered_to, ChannelId(format!("slack://{APP_ID}")));

    // The mock proves what crossed the wire: a POST carrying the Bearer token
    // and the {channel,text} body.
    let requests = server
        .received_requests()
        .await
        .expect("the mock recorded requests");
    assert_eq!(requests.len(), 1, "exactly one chat.postMessage was sent");
    let sent = &requests[0];
    assert_eq!(
        sent.headers
            .get("authorization")
            .expect("the request carries an Authorization header"),
        &format!("Bearer {BOT_TOKEN}")
    );
    let sent_body: serde_json::Value =
        serde_json::from_slice(&sent.body).expect("the request body is JSON");
    assert_eq!(sent_body["channel"], "C12345");
    assert_eq!(sent_body["text"], "hello from ardur");

    // ---- 2. Inbound: synthesize a signed Slack event and parse it.
    let ts = NOW_UNIX.to_string();
    let event_body = serde_json::json!({
        "type": "event_callback",
        "event": {
            "type": "message",
            "user": "U777",
            "text": "reply from a human",
            "channel": "C12345",
            "ts": "1700000010.000500"
        }
    })
    .to_string();
    let headers = SlackHeaders::new(sign(&ts, &event_body), &ts);

    let parsed = adapter
        .parse_event_at(&headers, &event_body, NOW_UNIX)
        .expect("the signed inbound event verifies and parses");

    let SlackEvent::Message(incoming) = parsed else {
        panic!("expected an inbound Message, got {parsed:?}");
    };
    assert_eq!(incoming.sender, SenderRef("U777".to_string()));
    assert_eq!(
        incoming.body,
        MessageBody::Text("reply from a human".to_string())
    );
    assert_eq!(
        incoming.channel_id,
        ChannelId(format!("slack://{APP_ID}/C12345"))
    );
}
