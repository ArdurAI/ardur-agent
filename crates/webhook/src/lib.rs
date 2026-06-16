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

pub mod error;
pub mod event;
mod inbound;
pub mod outbound;
pub mod signature;

pub use error::{WebhookError, Result};
pub use event::{WebhookEvent, EventType};
pub use inbound::{receive_webhook, WebhookConfig, InboundWebhookHandler, InboundState};
pub use outbound::{OutboundWebhookClient, OutboundWebhookConfig};
pub use signature::{verify_signature, sign_body};
