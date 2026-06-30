//! ardur-messaging-gateway — the channel abstraction the runtime sends and
//! receives messages through.
//!
//! Plan family: §4.0 (`plans/4.0-messaging-gateway-blueprint.md`).
//!
//! # Phase 1 (this crate)
//!
//! - [`MessagingGateway`] — the object-safe trait every channel backend
//!   implements: an async [`MessagingGateway::send_message`] and
//!   [`MessagingGateway::receive`], plus [`MessagingGateway::channel_id`] and
//!   [`MessagingGateway::supports_threading`].
//! - [`OutgoingMessage`] / [`IncomingMessage`] / [`MessageReceipt`] — the send,
//!   receive, and acceptance envelopes. A receipt's
//!   [`receipt_id`](MessageReceipt::receipt_id) bridges to the §11.14 receipt
//!   chain.
//! - [`MessageBody`] — the content taxonomy ([`Text`], [`Markdown`],
//!   [`Attachment`], [`Mention`]); [`MessageTarget`] — the addressing taxonomy
//!   ([`User`], [`Channel`], [`Thread`]). Both serialize adjacently tagged.
//! - [`InProcessGateway`] — the Phase-1 backend: an in-memory loopback over a
//!   [`tokio::sync::mpsc`] channel.
//! - [`GatewayRegistry`] — channel-id → gateway resolution.
//! - [`GatewayError`] / [`RegistryError`] — the typed failure surfaces.
//!
//! [`CapTokenRef`] is re-exported from `ardur-runtime`: an outgoing message is
//! authorized by the same capability handle a runtime turn is, so the two
//! share one type rather than redefining placeholders that would later have to
//! be reconciled.
//!
//! Phase 2 (see the inline `// TODO §4.0 Phase 2:` markers) adds the real
//! Slack / Signal / Discord wire adapters, genuine threading, attachment
//! offload to S3, cap-token verification at send, and provider-assigned
//! message ids.
//!
//! [`Text`]: MessageBody::Text
//! [`Markdown`]: MessageBody::Markdown
//! [`Attachment`]: MessageBody::Attachment
//! [`Mention`]: MessageBody::Mention
//! [`User`]: MessageTarget::User
//! [`Channel`]: MessageTarget::Channel
//! [`Thread`]: MessageTarget::Thread
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod gateway;
mod registry;
mod types;
mod verb;

pub use error::{GatewayError, RegistryError};
pub use gateway::{InProcessGateway, MessagingGateway};
pub use registry::GatewayRegistry;
pub use types::{
    ChannelId, ChannelRef, IncomingMessage, MessageBody, MessageReceipt, MessageTarget,
    OutgoingMessage, SenderRef, ThreadId, ThreadRef, UnixTsMillis, UserRef,
};
pub use verb::{
    MESSAGE_DELETED_EVENT, MESSAGE_EDITED_EVENT, MESSAGE_FORWARDED_EVENT, MESSAGE_OP_REFUSED_EVENT,
    MESSAGE_PINNED_EVENT, MESSAGE_QUOTED_EVENT, MESSAGE_REACTED_EVENT, MESSAGE_SENT_EVENT,
    MESSAGE_UNPINNED_EVENT, MessageVerb, MessageVerbRequest,
};

// The capability handle an outgoing message is authorized by is owned by §1.0;
// re-export so the gateway and the runtime share one schema.
pub use ardur_runtime::CapTokenRef;
