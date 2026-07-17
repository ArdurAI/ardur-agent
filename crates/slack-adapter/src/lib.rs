//! ardur-slack-adapter — the first real channel backend behind the §4.0
//! [`MessagingGateway`] contract.
//!
//! Plan family: §4.1 (Slack adapter Phase 1).
//!
//! # Phase 1 (this crate)
//!
//! - [`SlackAdapter`] — implements [`MessagingGateway`]. Construct with
//!   [`SlackAdapter::new`] (explicit bot token / signing secret / app id) or
//!   [`SlackAdapter::from_env`] (`SLACK_BOT_TOKEN` / `SLACK_SIGNING_SECRET` /
//!   `SLACK_APP_ID`). [`with_base_url`](SlackAdapter::with_base_url) repoints the
//!   Web-API host at a mock for tests.
//! - **Outbound**: [`MessagingGateway::send_message`] POSTs `chat.postMessage`
//!   with a `Bearer` bot token and returns a [`MessageReceipt`] whose
//!   `provider_message_id` is Slack's `ts`. The richer
//!   [`SlackAdapter::post_message`] is the native send that preserves the full
//!   [`SlackError`] taxonomy.
//! - **Inbound**: [`SlackAdapter::parse_event`] verifies the signing-secret
//!   HMAC in constant time, rejects stale timestamps outside the ±5-minute
//!   replay window, rejects duplicate signed deliveries inside that window, and
//!   parses the body into a [`SlackEvent`] — the `url_verification` handshake, a
//!   real [`SlackEvent::Message`], or an [`Ignored`](SlackEvent::Ignored)
//!   bot-own message.
//! - [`SlackError`] — the typed failure surface;
//!   [`into_gateway_error`](SlackError::into_gateway_error) lowers it onto
//!   [`GatewayError`] at the trait boundary.
//!
//! # Adapt-points vs. the §4.1 task brief
//!
//! - The task named a `ChannelAdapter` trait returning `MessageId` /
//!   `ChannelError`; the substrate on `dev` actually exposes
//!   [`MessagingGateway`] returning [`MessageReceipt`] / [`GatewayError`], so the
//!   adapter implements that. The named Slack error mappings are preserved on
//!   [`SlackError`] and lowered to the coarser [`GatewayError`] variants in
//!   [`SlackError::into_gateway_error`].
//! - `parse_event` returns a [`SlackEvent`] enum rather than a bare
//!   [`IncomingMessage`], because the handshake and bot-own-filter cases have no
//!   message to return.
//! - Inbound is webhook-push, so [`MessagingGateway::receive`] returns
//!   [`GatewayError::UnsupportedFeature`]; callers feed request bodies to
//!   [`parse_event`](SlackAdapter::parse_event).
//!
//! Phase 2 adds OAuth token exchange, threaded delivery, attachment offload,
//! and reactions.
//!
//! [`MessagingGateway`]: ardur_messaging_gateway::MessagingGateway
//! [`MessageReceipt`]: ardur_messaging_gateway::MessageReceipt
//! [`GatewayError`]: ardur_messaging_gateway::GatewayError
//! [`IncomingMessage`]: ardur_messaging_gateway::IncomingMessage
//! [`MessagingGateway::send_message`]: ardur_messaging_gateway::MessagingGateway::send_message
//! [`MessagingGateway::receive`]: ardur_messaging_gateway::MessagingGateway::receive
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod adapter;
mod error;
mod event;

pub use adapter::SlackAdapter;
pub use error::SlackError;
pub use event::{SlackEvent, SlackHeaders};
