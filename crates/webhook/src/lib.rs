//! ardur-webhook — inbound/outbound webhook surface with HTTP handlers,
//! signature verification (HMAC-SHA256), and event emission.
//!
//! Plan family: §4.X / §6.X webhook layer, §9.7 operator webhook surface.
//!
//! # Overview
//!
//! Substrate primitives:
//! - [`WebhookEvent`] — a normalized event envelope with id, timestamp,
//!   payload, and source.
//! - [`InboundWebhookHandler`] — trait for processing inbound webhooks.
//! - [`OutboundWebhookClient`] — client for sending signed webhooks with retry.
//! - [`WebhookError`] — typed error surface for the crate.
//!
//! # Operator surface (§9.7)
//!
//! [`WebhookOps`] is the gated operator facade over an outbound endpoint
//! registry and an inbound trigger registry, plus a signed emit path. Every
//! action admits through a cap-token scope check ([`CapGate`]) and emits a
//! signed, hash-chained receipt ([`ReceiptSink`]). Endpoints and triggers are
//! owner-scoped; HMAC secrets are referenced by environment-variable name and
//! never persisted in plaintext.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Outbound endpoint registry domain (§9.7).
pub mod endpoint;
/// Error types for webhook operations.
pub mod error;
/// Normalized webhook event types and variants.
pub mod event;
/// Cap-token gating + receipt emission for the operator surface (§9.7).
pub mod gate;
mod inbound;
/// The gated operator webhook facade (§9.7).
pub mod ops;
/// Durable JSON collection store for the operator surface (§9.7).
pub mod opstore;
/// Outbound webhook delivery client and configuration.
pub mod outbound;
/// Webhook signature computation and verification.
pub mod signature;
/// Inbound webhook trigger registry domain (§9.7).
pub mod trigger;

pub use endpoint::{EndpointRegistration, EndpointUpdate, OutboundEndpoint};
pub use error::{Result, WebhookError};
pub use event::{EventType, WebhookEvent};
pub use gate::{
    CapGate, Es256ReceiptSink, InMemoryReceiptSink, Principal, ReceiptEvent, ReceiptSink,
    RecordedReceipt, SCOPE_ENDPOINT_READ, SCOPE_ENDPOINT_REGISTER, SCOPE_INBOUND_REGISTER,
    SCOPE_OUTBOUND_EMIT, fingerprint,
};
pub use inbound::{InboundState, InboundWebhookHandler, WebhookConfig, receive_webhook};
pub use ops::{
    DispatchRequest, DispatchResult, Dispatcher, EmitReport, IDEMPOTENCY_HEADER, NONCE_HEADER,
    WebhookOps,
};
pub use opstore::{Identified, JsonCollectionStore};
pub use outbound::{OutboundWebhookClient, OutboundWebhookConfig};
pub use signature::{sign_body, verify_signature};
pub use trigger::{InboundTrigger, TriggerRegistration};
