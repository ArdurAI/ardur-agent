//! ardur-webhook — inbound/outbound webhook surface with HTTP handlers,
//! signature verification (HMAC-SHA256), and event emission.
//!
//! Plan family: §4.X / §6.X webhook layer.
//!
//! # Overview
//!
//! - [`WebhookEvent`] — a normalized event envelope with id, timestamp,
//!   payload, and source.
//! - [`EventPayload`] — common event type variants (JSON, text, generic).
//! - [`InboundWebhookHandler`] — trait for processing inbound webhooks.
//! - [`OutboundWebhookClient`] — client for sending signed webhooks with retry.
//! - [`WebhookRegistry`] — register endpoints and route by path or event type.
//! - [`WebhookError`] — typed error surface for the crate.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Error types for webhook operations.
pub mod error;
/// Normalized webhook event types and variants.
pub mod event;
mod inbound;
/// Outbound webhook delivery client and configuration.
pub mod outbound;
/// Registry mapping paths/sources to inbound handlers, and the axum
/// [`Router`](registry::WebhookRegistry::router) it mounts them under.
pub mod registry;
/// Webhook signature computation and verification.
pub mod signature;

pub use error::{Result, WebhookError};
pub use event::{EventType, WebhookEvent};
pub use inbound::{InboundState, InboundWebhookHandler, WebhookConfig, receive_webhook};
pub use outbound::{OutboundWebhookClient, OutboundWebhookConfig};
pub use registry::{WebhookEndpoint, WebhookRegistry};
pub use signature::{sign_body, verify_signature};
