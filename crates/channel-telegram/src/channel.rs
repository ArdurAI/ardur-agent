//! [`TelegramChannel`] — the Telegram backend for the §4.0 [`MessagingGateway`]
//! contract.
//!
//! Like the Matrix and Discord adapters, Telegram is a long-poll protocol — so
//! [`MessagingGateway::receive`] is a real method here:
//! [`start`](TelegramChannel::start) runs a teloxide [`Dispatcher`] on a Tokio
//! task whose message endpoint forwards each inbound text message onto an
//! internal [`mpsc`] queue (a repl-style single-endpoint handler tree), and
//! `receive` pops the next one off it. Outbound sends go through a cloned
//! [`Bot`], so they work whether or not the dispatcher is running.

use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use secrecy::ExposeSecret;
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use ardur_messaging_gateway::{
    ChannelId, GatewayError, IncomingMessage, MessageBody, MessageReceipt, MessageTarget,
    MessagingGateway, OutgoingMessage, SenderRef, UnixTsMillis,
};

use teloxide::prelude::*;
use teloxide::types::ChatId;

use crate::config::TelegramConfig;
use crate::error::TelegramError;

/// A Telegram channel adapter: sends plaintext chat messages and forwards
/// inbound chat text messages through the gateway.
///
/// Construct with [`TelegramChannel::new`] (builds the bot and validates the
/// token via `get_me`), then call [`start`](Self::start) once to begin draining
/// inbound traffic. Hold it behind `dyn MessagingGateway` to send and
/// [`receive`](MessagingGateway::receive).
pub struct TelegramChannel {
    bot: Bot,
    channel_id: ChannelId,
    /// Inbound forwarding context shared with the dispatcher's message endpoint.
    forwarder: Forwarder,
    /// The drain side of the inbound queue. `receive(&self)` needs `&mut` access
    /// to `recv`, so a Mutex hands out exclusive access behind the shared ref.
    inbound_rx: Mutex<mpsc::UnboundedReceiver<IncomingMessage>>,
    /// Set once [`start`](Self::start) has spawned the dispatcher, so a second
    /// call is a no-op rather than a second polling loop (which Telegram rejects
    /// with a 409 conflict).
    started: AtomicBool,
}

/// The clone-able context the dispatcher endpoint runs against: where to forward
/// an inbound message, the allowlist to gate it by, the bot's own user id (echo
/// prevention), and the namespaced channel-id prefix.
#[derive(Clone)]
struct Forwarder {
    tx: mpsc::UnboundedSender<IncomingMessage>,
    allowed_chats: Arc<HashSet<i64>>,
    bot_id: u64,
    channel_prefix: String,
}

impl Forwarder {
    /// Whether `chat_id` is permitted (empty allowlist = all chats).
    fn chat_allowed(&self, chat_id: i64) -> bool {
        self.allowed_chats.is_empty() || self.allowed_chats.contains(&chat_id)
    }

    /// Gate, echo-filter, and forward one inbound Telegram message.
    fn on_message(&self, msg: &Message) {
        // Echo prevention: never re-ingest a message the bot itself sent.
        if msg.from.as_ref().is_some_and(|u| u.id.0 == self.bot_id) {
            return;
        }
        let chat_id = msg.chat.id.0;
        if !self.chat_allowed(chat_id) {
            tracing::warn!(
                chat = chat_id,
                "dropping telegram message from a chat outside the allowlist"
            );
            return;
        }
        // Phase 1 forwards only text messages; non-text updates (photos, stickers,
        // …) need the media path and are ignored.
        let Some(text) = msg.text() else {
            return;
        };

        let sender = msg
            .from
            .as_ref()
            .map_or_else(|| "unknown".to_owned(), |u| u.id.0.to_string());
        let incoming = IncomingMessage {
            message_id: Uuid::new_v4(),
            channel_id: ChannelId(format!("{}/{chat_id}", self.channel_prefix)),
            sender: SenderRef(sender),
            body: MessageBody::Text(text.to_owned()),
            received_at: msg.date.timestamp_millis().max(0) as u64,
            thread_id: None,
        };

        if self.tx.send(incoming).is_err() {
            tracing::error!(
                chat = chat_id,
                "telegram inbound receiver is gone; dropping message"
            );
        }
    }
}

impl TelegramChannel {
    /// Build the bot and validate the token (a `get_me` call that also yields the
    /// bot's own user id, used for echo prevention).
    ///
    /// This does **not** start polling — call [`start`](Self::start) once
    /// afterwards.
    ///
    /// # Errors
    /// [`TelegramError::Connect`] if `get_me` fails (rejected token, no network).
    pub async fn new(config: TelegramConfig) -> Result<Self, TelegramError> {
        let bot = Bot::new(config.bot_token.expose_secret());
        let me = bot
            .get_me()
            .await
            .map_err(|e| TelegramError::Connect(e.to_string()))?;
        let bot_id = me.user.id.0;

        let (tx, rx) = mpsc::unbounded_channel();
        let channel_prefix = format!("telegram://{bot_id}");
        let channel_id = ChannelId(channel_prefix.clone());
        let forwarder = Forwarder {
            tx,
            allowed_chats: Arc::new(config.allowed_chat_ids.iter().copied().collect()),
            bot_id,
            channel_prefix,
        };

        Ok(Self {
            bot,
            channel_id,
            forwarder,
            inbound_rx: Mutex::new(rx),
            started: AtomicBool::new(false),
        })
    }

    /// Build the single-endpoint handler tree and spawn the teloxide dispatcher.
    ///
    /// Idempotency is enforced: the first call spawns the polling loop; a second
    /// call is a logged no-op (a second long-poll would 409-conflict on
    /// Telegram). The spawned task runs until the process exits.
    pub fn start(&self) {
        if self.started.swap(true, Ordering::SeqCst) {
            tracing::warn!("telegram channel already started; ignoring the second start");
            return;
        }

        let forwarder = self.forwarder.clone();
        let bot = self.bot.clone();
        // A repl-style single endpoint: every message update runs `on_message`.
        let handler = Update::filter_message().endpoint(move |msg: Message| {
            let forwarder = forwarder.clone();
            async move {
                forwarder.on_message(&msg);
                Ok::<(), Infallible>(())
            }
        });

        tokio::spawn(async move {
            Dispatcher::builder(bot, handler).build().dispatch().await;
        });
    }

    /// Send plaintext to a chat, returning the Telegram-assigned message id.
    ///
    /// The adapter's native send — preserves the full [`TelegramError`] taxonomy
    /// the trait lowers onto the coarser [`GatewayError`].
    ///
    /// # Errors
    /// - [`TelegramError::InvalidChatId`] if `chat_id` is not an `i64`.
    /// - [`TelegramError::Send`] if Telegram rejects the send.
    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<String, TelegramError> {
        let parsed = chat_id
            .parse::<i64>()
            .map_err(|_| TelegramError::InvalidChatId(chat_id.to_owned()))?;
        let sent = self
            .bot
            .send_message(ChatId(parsed), text)
            .await
            .map_err(|e| TelegramError::Send(e.to_string()))?;
        Ok(sent.id.0.to_string())
    }

    /// Resolve an [`OutgoingMessage`] target into a chat id string.
    fn target_chat(target: &MessageTarget) -> Result<String, TelegramError> {
        match target {
            MessageTarget::Channel(c) => Ok(c.0.clone()),
            MessageTarget::User(_) => Err(TelegramError::UnsupportedTarget(
                "telegram adapter Phase 1 addresses chats by id, not user handles".to_owned(),
            )),
            MessageTarget::Thread(_) => Err(TelegramError::UnsupportedTarget(
                "telegram adapter Phase 1 cannot deliver into a thread".to_owned(),
            )),
        }
    }

    /// Render an [`OutgoingMessage`] body into the plaintext a message carries.
    fn body_text(body: &MessageBody) -> Result<String, TelegramError> {
        match body {
            MessageBody::Text(t) | MessageBody::Markdown(t) => Ok(t.clone()),
            MessageBody::Mention { user_ref, body } => Ok(format!("{} {}", user_ref.0, body)),
            MessageBody::Attachment { .. } => Err(TelegramError::UnsupportedTarget(
                "telegram adapter Phase 1 cannot send attachments".to_owned(),
            )),
        }
    }
}

#[async_trait]
impl MessagingGateway for TelegramChannel {
    async fn send_message(&self, msg: OutgoingMessage) -> Result<MessageReceipt, GatewayError> {
        let chat = Self::target_chat(&msg.target).map_err(|e| e.into_gateway_error(""))?;
        let text = Self::body_text(&msg.body).map_err(|e| e.into_gateway_error(&chat))?;

        let message_id = self
            .send_text(&chat, &text)
            .await
            .map_err(|e| e.into_gateway_error(&chat))?;

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
        // `forwarder`, so this cannot happen while `self` is alive.
        rx.recv().await.ok_or_else(|| {
            GatewayError::DeliveryFailed("telegram inbound channel closed".to_owned())
        })
    }

    fn channel_id(&self) -> ChannelId {
        self.channel_id.clone()
    }

    fn supports_threading(&self) -> bool {
        // Forum-topic / reply threading is Phase 2.
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
