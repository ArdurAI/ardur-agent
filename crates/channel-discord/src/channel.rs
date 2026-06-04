//! [`DiscordChannel`] — the Discord backend for the §4.0 [`MessagingGateway`]
//! contract.
//!
//! Like the Matrix adapter (and unlike the webhook-push Slack adapter), Discord
//! is a long-poll gateway protocol — so [`MessagingGateway::receive`] is a real
//! method here: [`start`](DiscordChannel::start) runs the serenity gateway loop
//! on a Tokio task whose `message` event handler forwards each inbound text
//! message onto an internal [`mpsc`] queue, and `receive` pops the next one off
//! it. Outbound sends go through a cloned [`Http`] client, so they work whether
//! or not the gateway loop is running.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use secrecy::ExposeSecret;
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use ardur_messaging_gateway::{
    ChannelId, GatewayError, IncomingMessage, MessageBody, MessageReceipt, MessageTarget,
    MessagingGateway, OutgoingMessage, SenderRef, UnixTsMillis,
};

use serenity::all::{
    ChannelId as DiscordChannelId, Client, Context, EventHandler, GatewayIntents, Http, Message,
    Ready,
};

use crate::config::DiscordConfig;
use crate::error::DiscordError;

/// A Discord channel adapter: sends plaintext channel messages and forwards
/// inbound channel/DM text messages through the gateway.
///
/// Construct with [`DiscordChannel::new`] (which builds the serenity client),
/// then call [`start`](Self::start) once to begin draining inbound traffic. Hold
/// it behind `dyn MessagingGateway` to send and [`receive`](MessagingGateway::receive).
pub struct DiscordChannel {
    /// The HTTP client, cloned from the gateway client, used for outbound sends.
    http: Arc<Http>,
    /// The serenity gateway client, moved out by the first [`start`](Self::start).
    client: Mutex<Option<Client>>,
    channel_id: ChannelId,
    /// The drain side of the inbound queue. `receive(&self)` needs `&mut` access
    /// to `recv`, so a Mutex hands out exclusive access behind the shared ref.
    inbound_rx: Mutex<mpsc::UnboundedReceiver<IncomingMessage>>,
}

/// The clone-able context the serenity `message` handler runs against: where to
/// forward an inbound message, the allowlist to gate it by, the bot's own id
/// (echo prevention), and the namespaced channel-id prefix.
#[derive(Clone)]
struct Forwarder {
    tx: mpsc::UnboundedSender<IncomingMessage>,
    allowed_channels: Arc<HashSet<u64>>,
    /// The bot's own user id (== its application id). An inbound message whose
    /// author is this id is the bot's own message and is dropped.
    bot_id: u64,
    channel_prefix: String,
}

impl Forwarder {
    /// Whether `channel_id` is permitted (empty allowlist = all channels).
    fn channel_allowed(&self, channel_id: u64) -> bool {
        self.allowed_channels.is_empty() || self.allowed_channels.contains(&channel_id)
    }

    /// Gate, echo-filter, and forward one inbound Discord message.
    fn on_message(&self, msg: &Message) {
        // Echo prevention: never re-ingest a message the bot itself sent.
        if msg.author.id.get() == self.bot_id {
            return;
        }
        let channel = msg.channel_id.get();
        if !self.channel_allowed(channel) {
            tracing::warn!(
                channel,
                "dropping discord message from a channel outside the allowlist"
            );
            return;
        }
        // Phase 1 forwards only message text; with `MESSAGE_CONTENT` enabled this
        // is the user's content. An empty content (e.g. an attachment-only
        // message) has nothing for the runtime to answer, so it is ignored.
        if msg.content.is_empty() {
            return;
        }

        let incoming = IncomingMessage {
            message_id: Uuid::new_v4(),
            channel_id: ChannelId(format!("{}/{channel}", self.channel_prefix)),
            sender: SenderRef(msg.author.id.to_string()),
            body: MessageBody::Text(msg.content.clone()),
            // serenity's `Timestamp` exposes whole-second Unix time; scale to the
            // gateway's millisecond convention.
            received_at: (msg.timestamp.unix_timestamp().max(0) as u64) * 1000,
            thread_id: None,
        };

        if self.tx.send(incoming).is_err() {
            tracing::error!(
                channel,
                "discord inbound receiver is gone; dropping message"
            );
        }
    }
}

/// The serenity event handler: forwards inbound messages and logs the gateway
/// `ready` handshake.
struct Handler {
    forwarder: Forwarder,
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, _ctx: Context, new_message: Message) {
        self.forwarder.on_message(&new_message);
    }

    async fn ready(&self, _ctx: Context, data_about_bot: Ready) {
        tracing::info!(
            bot = %data_about_bot.user.name,
            "discord gateway ready"
        );
    }
}

impl DiscordChannel {
    /// Build the serenity client and capture its HTTP handle.
    ///
    /// This does **not** connect the gateway — call [`start`](Self::start) once
    /// afterwards. Construction is local (no network); the gateway connection and
    /// token validation happen in `start`.
    ///
    /// # Errors
    /// [`DiscordError::Connect`] if the serenity client cannot be built.
    pub async fn new(config: DiscordConfig) -> Result<Self, DiscordError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let channel_prefix = format!("discord://{}", config.application_id);
        let channel_id = ChannelId(channel_prefix.clone());
        let forwarder = Forwarder {
            tx,
            allowed_channels: Arc::new(config.allowed_channel_ids.iter().copied().collect()),
            bot_id: config.application_id,
            channel_prefix,
        };

        let intents: GatewayIntents = config.intents;
        let client = Client::builder(config.bot_token.expose_secret(), intents)
            .event_handler(Handler { forwarder })
            .await
            .map_err(|e| DiscordError::Connect(e.to_string()))?;

        let http = client.http.clone();

        Ok(Self {
            http,
            client: Mutex::new(Some(client)),
            channel_id,
            inbound_rx: Mutex::new(rx),
        })
    }

    /// Connect the gateway and spawn the serenity event loop.
    ///
    /// Idempotency is the caller's responsibility: the first call moves the
    /// client onto a spawned task; a second call is a logged no-op. The spawned
    /// task runs until the gateway errors or the process exits.
    pub async fn start(&self) {
        let Some(mut client) = self.client.lock().await.take() else {
            tracing::warn!("discord channel already started; ignoring the second start");
            return;
        };
        tokio::spawn(async move {
            if let Err(e) = client.start().await {
                tracing::error!(error = %e, "discord gateway loop exited with error");
            }
        });
    }

    /// Send plaintext to a channel, returning the Discord-assigned message id.
    ///
    /// The adapter's native send — preserves the full [`DiscordError`] taxonomy
    /// the trait lowers onto the coarser [`GatewayError`].
    ///
    /// # Errors
    /// - [`DiscordError::InvalidChannelId`] if `channel_id` is not a `u64`.
    /// - [`DiscordError::Send`] if Discord rejects the send (no access, …).
    pub async fn send_text(&self, channel_id: &str, text: &str) -> Result<String, DiscordError> {
        let parsed = channel_id
            .parse::<u64>()
            .map_err(|_| DiscordError::InvalidChannelId(channel_id.to_owned()))?;
        let sent = DiscordChannelId::new(parsed)
            .say(&self.http, text)
            .await
            .map_err(|e| DiscordError::Send(e.to_string()))?;
        Ok(sent.id.to_string())
    }

    /// Resolve an [`OutgoingMessage`] target into a channel id string.
    fn target_channel(target: &MessageTarget) -> Result<String, DiscordError> {
        match target {
            MessageTarget::Channel(c) => Ok(c.0.clone()),
            MessageTarget::User(_) => Err(DiscordError::UnsupportedTarget(
                "discord adapter Phase 1 cannot open a direct-message channel".to_owned(),
            )),
            MessageTarget::Thread(_) => Err(DiscordError::UnsupportedTarget(
                "discord adapter Phase 1 cannot deliver into a thread".to_owned(),
            )),
        }
    }

    /// Render an [`OutgoingMessage`] body into the plaintext a message carries.
    fn body_text(body: &MessageBody) -> Result<String, DiscordError> {
        match body {
            MessageBody::Text(t) | MessageBody::Markdown(t) => Ok(t.clone()),
            MessageBody::Mention { user_ref, body } => Ok(format!("<@{}> {}", user_ref.0, body)),
            MessageBody::Attachment { .. } => Err(DiscordError::UnsupportedTarget(
                "discord adapter Phase 1 cannot send attachments".to_owned(),
            )),
        }
    }
}

#[async_trait]
impl MessagingGateway for DiscordChannel {
    async fn send_message(&self, msg: OutgoingMessage) -> Result<MessageReceipt, GatewayError> {
        let channel = Self::target_channel(&msg.target).map_err(|e| e.into_gateway_error(""))?;
        let text = Self::body_text(&msg.body).map_err(|e| e.into_gateway_error(&channel))?;

        let message_id = self
            .send_text(&channel, &text)
            .await
            .map_err(|e| e.into_gateway_error(&channel))?;

        Ok(MessageReceipt {
            delivered_to: msg.channel_id,
            delivered_at: now_millis(),
            provider_message_id: Some(message_id),
            // Reuse the caller's correlation id so the delivery binds to its run.
            receipt_id: msg.message_id,
        })
    }

    async fn receive(&self) -> Result<IncomingMessage, GatewayError> {
        let mut rx = self.inbound_rx.lock().await;
        // `None` only if every sender dropped; the channel holds the `tx` inside
        // the spawned handler, so this cannot happen while the loop is alive.
        rx.recv().await.ok_or_else(|| {
            GatewayError::DeliveryFailed("discord inbound channel closed".to_owned())
        })
    }

    fn channel_id(&self) -> ChannelId {
        self.channel_id.clone()
    }

    fn supports_threading(&self) -> bool {
        // Threaded replies are Phase 2.
        false
    }
}

/// Current wall-clock time in Unix milliseconds (saturating to 0 before the epoch).
fn now_millis() -> UnixTsMillis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
