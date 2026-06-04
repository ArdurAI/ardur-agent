//! [`MatrixChannel`] — the Matrix backend for the §4.0 [`MessagingGateway`]
//! contract.
//!
//! Unlike the webhook-push Slack adapter, Matrix is a genuinely bidirectional,
//! long-poll protocol — so [`MessagingGateway::receive`] is a real method here:
//! [`start_sync`](MatrixChannel::start_sync) runs the SDK's sync loop on a Tokio
//! task whose event handler forwards each inbound room message onto an internal
//! [`mpsc`] queue, and `receive` pops the next one off it (the same loopback
//! shape as the gateway's [`InProcessGateway`], but fed by real traffic).

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

use matrix_sdk::Client;
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::event_handler::Ctx;
use matrix_sdk::room::Room;
use matrix_sdk::ruma::events::room::member::{MembershipState, StrippedRoomMemberEvent};
use matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
};
use matrix_sdk::ruma::{OwnedUserId, RoomId, UserId};
use matrix_sdk::{SessionMeta, SessionTokens};

use crate::config::MatrixConfig;
use crate::error::MatrixError;

/// A Matrix channel adapter: sends plaintext room messages and forwards inbound
/// room text events through the gateway.
///
/// Construct with [`MatrixChannel::new`] (which builds the SDK client and
/// restores the bot session), then call [`start_sync`](Self::start_sync) once to
/// begin draining inbound traffic. Hold it behind `dyn MessagingGateway` to send
/// and [`receive`](MessagingGateway::receive).
pub struct MatrixChannel {
    client: Client,
    user_id: OwnedUserId,
    channel_id: ChannelId,
    /// Inbound forwarding context shared with the sync event handlers.
    forwarder: Forwarder,
    /// The drain side of the inbound queue. `receive(&self)` needs `&mut` access
    /// to `recv`, so a Mutex hands out exclusive access behind the shared ref.
    inbound_rx: Mutex<mpsc::UnboundedReceiver<IncomingMessage>>,
}

/// The clone-able context the SDK event handlers run against: where to forward an
/// inbound message, the allowlist to gate it by, this bot's own id (echo
/// prevention), and the namespaced channel-id prefix.
#[derive(Clone)]
struct Forwarder {
    tx: mpsc::UnboundedSender<IncomingMessage>,
    allowed_rooms: Arc<HashSet<String>>,
    user_id: OwnedUserId,
    channel_prefix: String,
    auto_join_invites: bool,
}

impl Forwarder {
    /// Whether `room_id` is permitted (empty allowlist = all rooms).
    fn room_allowed(&self, room_id: &str) -> bool {
        self.allowed_rooms.is_empty() || self.allowed_rooms.contains(room_id)
    }
}

impl MatrixChannel {
    /// Build the client, open the sqlite state/crypto store, and restore the bot
    /// session from the configured access token.
    ///
    /// This does **not** start syncing — call [`start_sync`](Self::start_sync)
    /// once afterwards.
    ///
    /// # Errors
    /// - [`MatrixError::InvalidUserId`] if `config.user_id` is malformed.
    /// - [`MatrixError::Connect`] if the client cannot be built (bad homeserver
    ///   URL, unwritable state dir) or the session is rejected.
    pub async fn new(config: MatrixConfig) -> Result<Self, MatrixError> {
        let user_id = UserId::parse(&config.user_id)
            .map_err(|_| MatrixError::InvalidUserId(config.user_id.clone()))?;
        let device_id = config.resolved_device_id().to_owned();

        let client = Client::builder()
            .homeserver_url(&config.homeserver_url)
            .sqlite_store(&config.state_dir, None)
            .build()
            .await
            .map_err(|e| MatrixError::Connect(e.to_string()))?;

        let session = MatrixSession {
            meta: SessionMeta {
                user_id: user_id.clone(),
                device_id: device_id.as_str().into(),
            },
            tokens: SessionTokens {
                access_token: config.access_token.expose_secret().to_owned(),
                refresh_token: None,
            },
        };
        client
            .restore_session(session)
            .await
            .map_err(|e| MatrixError::Connect(e.to_string()))?;

        let (tx, rx) = mpsc::unbounded_channel();
        let channel_id = ChannelId(format!("matrix://{user_id}"));
        let forwarder = Forwarder {
            tx,
            allowed_rooms: Arc::new(config.allowed_rooms.iter().cloned().collect()),
            user_id: user_id.clone(),
            channel_prefix: format!("matrix://{user_id}"),
            auto_join_invites: config.auto_join_invites,
        };

        Ok(Self {
            client,
            user_id,
            channel_id,
            forwarder,
            inbound_rx: Mutex::new(rx),
        })
    }

    /// Register the inbound event handlers and spawn the SDK sync loop.
    ///
    /// Idempotency is the caller's responsibility: call this exactly once after
    /// [`new`](Self::new). The spawned task runs until the client errors or the
    /// process exits.
    pub fn start_sync(&self) {
        // The handlers retrieve their context via `Ctx<Forwarder>`.
        self.client
            .add_event_handler_context(self.forwarder.clone());
        self.client.add_event_handler(on_room_message);
        self.client.add_event_handler(on_stripped_member);

        let client = self.client.clone();
        tokio::spawn(async move {
            if let Err(e) = client.sync(SyncSettings::default()).await {
                tracing::error!(error = %e, "matrix sync loop exited with error");
            }
        });
    }

    /// The bot's own Matrix user id.
    #[must_use]
    pub fn user_id(&self) -> &str {
        self.user_id.as_str()
    }

    /// Send plaintext to a room, returning the homeserver-assigned event id.
    ///
    /// The adapter's native send — preserves the full [`MatrixError`] taxonomy
    /// the trait lowers onto the coarser [`GatewayError`].
    ///
    /// # Errors
    /// - [`MatrixError::InvalidRoomId`] if `room_id` is malformed.
    /// - [`MatrixError::RoomNotFound`] if the bot is not joined to the room.
    /// - [`MatrixError::Send`] if the homeserver rejects the send.
    pub async fn send_text(&self, room_id: &str, text: &str) -> Result<String, MatrixError> {
        let parsed =
            RoomId::parse(room_id).map_err(|_| MatrixError::InvalidRoomId(room_id.to_owned()))?;
        let room = self
            .client
            .get_room(&parsed)
            .ok_or_else(|| MatrixError::RoomNotFound(room_id.to_owned()))?;
        let content = RoomMessageEventContent::text_plain(text);
        let resp = room
            .send(content)
            .await
            .map_err(|e| MatrixError::Send(e.to_string()))?;
        Ok(resp.response.event_id.to_string())
    }

    /// Resolve an [`OutgoingMessage`] target into a room id string.
    fn target_room(target: &MessageTarget) -> Result<String, MatrixError> {
        match target {
            // A Matrix room is the broadcast channel; `ChannelRef` carries its id.
            MessageTarget::Channel(c) => Ok(c.0.clone()),
            // Direct messages need a DM room create/lookup — Phase 2.
            MessageTarget::User(_) => Err(MatrixError::UnsupportedTarget(
                "matrix adapter Phase 1 cannot open a direct-message room".to_owned(),
            )),
            // Threaded replies (`m.thread` relation) — Phase 2.
            MessageTarget::Thread(_) => Err(MatrixError::UnsupportedTarget(
                "matrix adapter Phase 1 cannot deliver into a thread".to_owned(),
            )),
        }
    }

    /// Render an [`OutgoingMessage`] body into the plaintext a room message carries.
    fn body_text(body: &MessageBody) -> Result<String, MatrixError> {
        match body {
            MessageBody::Text(t) | MessageBody::Markdown(t) => Ok(t.clone()),
            MessageBody::Mention { user_ref, body } => Ok(format!("{} {}", user_ref.0, body)),
            // Inline attachment bytes need the Phase-2 media-upload path.
            MessageBody::Attachment { .. } => Err(MatrixError::UnsupportedTarget(
                "matrix adapter Phase 1 cannot send attachments".to_owned(),
            )),
        }
    }
}

#[async_trait]
impl MessagingGateway for MatrixChannel {
    async fn send_message(&self, msg: OutgoingMessage) -> Result<MessageReceipt, GatewayError> {
        let room_id = Self::target_room(&msg.target).map_err(|e| e.into_gateway_error(""))?;
        let text = Self::body_text(&msg.body).map_err(|e| e.into_gateway_error(&room_id))?;

        let event_id = self
            .send_text(&room_id, &text)
            .await
            .map_err(|e| e.into_gateway_error(&room_id))?;

        Ok(MessageReceipt {
            delivered_to: msg.channel_id,
            delivered_at: now_millis(),
            // Matrix's own message id is the event id it returned.
            provider_message_id: Some(event_id),
            // Reuse the caller's correlation id so the delivery binds to its run.
            receipt_id: msg.message_id,
        })
    }

    async fn receive(&self) -> Result<IncomingMessage, GatewayError> {
        let mut rx = self.inbound_rx.lock().await;
        // `None` only if every sender dropped; the channel holds the `tx` inside
        // `forwarder`, so this cannot happen while `self` is alive.
        rx.recv()
            .await
            .ok_or_else(|| GatewayError::DeliveryFailed("matrix inbound channel closed".to_owned()))
    }

    fn channel_id(&self) -> ChannelId {
        self.channel_id.clone()
    }

    fn supports_threading(&self) -> bool {
        // Threaded replies (`m.thread`) are Phase 2.
        false
    }
}

/// Inbound `m.room.message` handler: drops our own messages (echo prevention)
/// and out-of-allowlist rooms, then forwards a text message onto the queue.
async fn on_room_message(ev: OriginalSyncRoomMessageEvent, room: Room, ctx: Ctx<Forwarder>) {
    // Echo prevention: never re-ingest a message the bot itself sent.
    if ev.sender == ctx.user_id {
        return;
    }

    let room_id = room.room_id().as_str().to_owned();
    if !ctx.room_allowed(&room_id) {
        tracing::warn!(room = %room_id, "dropping matrix message from a room outside the allowlist");
        return;
    }

    // Phase 1 forwards only plain/formatted text bodies; other msgtypes (image,
    // file, …) need the media path and are ignored.
    let MessageType::Text(text) = ev.content.msgtype else {
        return;
    };

    let incoming = IncomingMessage {
        message_id: Uuid::new_v4(),
        channel_id: ChannelId(format!("{}/{}", ctx.channel_prefix, room_id)),
        sender: SenderRef(ev.sender.to_string()),
        body: MessageBody::Text(text.body),
        received_at: u64::from(ev.origin_server_ts.get()),
        thread_id: None,
    };

    if ctx.tx.send(incoming).is_err() {
        tracing::error!(room = %room_id, "matrix inbound receiver is gone; dropping message");
    }
}

/// Stripped room-member handler: auto-join an invite addressed to the bot when
/// [`MatrixConfig::auto_join_invites`] is set and the room clears the allowlist.
async fn on_stripped_member(
    ev: StrippedRoomMemberEvent,
    room: Room,
    client: Client,
    ctx: Ctx<Forwarder>,
) {
    if !ctx.auto_join_invites {
        return;
    }
    // Only react to an invite directed at *this* bot.
    let Some(me) = client.user_id() else { return };
    if ev.state_key != me || ev.content.membership != MembershipState::Invite {
        return;
    }
    let room_id = room.room_id().as_str().to_owned();
    if !ctx.room_allowed(&room_id) {
        tracing::warn!(room = %room_id, "declining matrix invite from a room outside the allowlist");
        return;
    }
    match room.join().await {
        Ok(()) => tracing::info!(room = %room_id, "auto-joined matrix room on invite"),
        Err(e) => tracing::error!(room = %room_id, error = %e, "failed to auto-join matrix room"),
    }
}

/// Current wall-clock time in Unix milliseconds (saturating to 0 before the epoch).
fn now_millis() -> UnixTsMillis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
