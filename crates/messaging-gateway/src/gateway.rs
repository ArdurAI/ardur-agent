//! The [`MessagingGateway`] trait and its Phase-1 [`InProcessGateway`] backend.

use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use crate::error::GatewayError;
use crate::types::{
    ChannelId, IncomingMessage, MessageReceipt, MessageTarget, OutgoingMessage, SenderRef,
    UnixTsMillis,
};
use crate::verb::{MessageVerb, MessageVerbRequest};

/// A bidirectional message channel: send an [`OutgoingMessage`] for delivery,
/// and long-poll for the next [`IncomingMessage`].
///
/// Object-safe via [`async_trait`] so a `Box<dyn MessagingGateway>` can live in
/// the [`GatewayRegistry`](crate::GatewayRegistry).
#[async_trait]
pub trait MessagingGateway: Send + Sync {
    /// Deliver a message, returning a [`MessageReceipt`] on acceptance.
    async fn send_message(&self, msg: OutgoingMessage) -> Result<MessageReceipt, GatewayError>;

    /// Dispatch a typed per-message operation.
    ///
    /// The compatibility implementation maps [`MessageVerb::Send`] onto the
    /// existing [`send_message`](Self::send_message) path. Other verbs refuse
    /// with a typed error until a backend implements the richer §4.11 contract.
    async fn dispatch_message_verb(
        &self,
        request: MessageVerbRequest,
    ) -> Result<MessageReceipt, GatewayError> {
        let MessageVerbRequest {
            operation_id,
            channel_id,
            target,
            verb,
            cap_token,
            parent_message_id,
        } = request;

        match verb {
            MessageVerb::Send { body } => {
                self.send_message(OutgoingMessage {
                    message_id: operation_id,
                    channel_id,
                    target,
                    body,
                    cap_token,
                    parent_message_id,
                })
                .await
            }
            verb => Err(GatewayError::MessageVerbUnsupported {
                verb: verb.id().to_owned(),
            }),
        }
    }

    /// Await the next inbound message. Long-poll style: resolves as soon as a
    /// message is available (immediately if one is already queued).
    async fn receive(&self) -> Result<IncomingMessage, GatewayError>;

    /// The channel this gateway serves — its registry key.
    fn channel_id(&self) -> ChannelId;

    /// Whether this channel can deliver into threads.
    fn supports_threading(&self) -> bool;
}

/// Current wall-clock time in Unix milliseconds (saturating to 0 before the
/// epoch, which cannot occur in practice).
fn now_millis() -> UnixTsMillis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// An in-memory gateway that loops sent messages straight back as inbound ones.
///
/// Phase 1's stand-in for a real wire backend: [`send_message`] echoes the
/// message onto an internal [`tokio::sync::mpsc`] channel, and [`receive`] pops
/// the next queued message off it. Useful for wiring the runtime against the
/// gateway contract before any provider adapter exists.
///
/// [`send_message`]: MessagingGateway::send_message
/// [`receive`]: MessagingGateway::receive
// TODO §4.0 Phase 2: real Slack / Signal / Discord adapters implementing this
// same trait against the providers' wire protocols.
pub struct InProcessGateway {
    channel_id: ChannelId,
    tx: mpsc::UnboundedSender<IncomingMessage>,
    // `receive(&self)` takes `&self`, but `UnboundedReceiver::recv` needs
    // `&mut self`; the Mutex hands out exclusive access behind the shared ref.
    rx: Mutex<mpsc::UnboundedReceiver<IncomingMessage>>,
}

impl InProcessGateway {
    /// Build an in-process gateway bound to `channel_id`.
    #[must_use]
    pub fn new(channel_id: ChannelId) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            channel_id,
            tx,
            rx: Mutex::new(rx),
        }
    }
}

#[async_trait]
impl MessagingGateway for InProcessGateway {
    async fn send_message(&self, msg: OutgoingMessage) -> Result<MessageReceipt, GatewayError> {
        // The in-process channel has no notion of threads.
        if matches!(msg.target, MessageTarget::Thread(_)) && !self.supports_threading() {
            return Err(GatewayError::UnsupportedFeature(
                "in-process gateway cannot deliver into a thread".to_owned(),
            ));
        }

        let delivered_to = msg.channel_id.clone();
        let echo = IncomingMessage {
            message_id: msg.message_id,
            channel_id: msg.channel_id,
            sender: SenderRef(self.channel_id.0.clone()),
            body: msg.body,
            received_at: now_millis(),
            thread_id: None,
        };
        self.tx
            .send(echo)
            .map_err(|e| GatewayError::DeliveryFailed(e.to_string()))?;

        Ok(MessageReceipt {
            delivered_to,
            delivered_at: now_millis(),
            // In-process delivery has no upstream provider to assign an id.
            provider_message_id: None,
            receipt_id: Uuid::new_v4(),
        })
    }

    async fn receive(&self) -> Result<IncomingMessage, GatewayError> {
        let mut rx = self.rx.lock().await;
        // `None` only if every sender dropped; the gateway holds `tx`, so this
        // cannot happen while `self` is alive.
        rx.recv()
            .await
            .ok_or_else(|| GatewayError::DeliveryFailed("receive channel closed".to_owned()))
    }

    fn channel_id(&self) -> ChannelId {
        self.channel_id.clone()
    }

    fn supports_threading(&self) -> bool {
        false
    }
}
