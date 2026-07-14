//! [`EmailChannel`] — the email backend for the §4.0 [`MessagingGateway`]
//! contract.
//!
//! Unlike the Discord/Telegram/Matrix adapters (which hold a long-lived
//! socket open to a gateway), IMAP is a request/response protocol without a
//! push mechanism this crate implements (`IDLE` is Phase 2 — see the crate
//! docs' adapt-points note). [`start`](EmailChannel::start) instead spawns a
//! blocking poll loop: connect, select `INBOX`, search `UNSEEN`, fetch each
//! new message, parse it, gate it through the sender allowlist, forward it
//! onto an internal queue, mark it `\Seen`, sleep, repeat. Outbound sends go
//! through a cloned SMTP transport, so they work whether or not the poll loop
//! is running.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message as LettreMessage, Tokio1Executor};
use secrecy::ExposeSecret;
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use ardur_messaging_gateway::{
    ChannelId, GatewayError, IncomingMessage, MessageBody, MessageReceipt, MessageTarget,
    MessagingGateway, OutgoingMessage, SenderRef, UnixTsMillis,
};

use crate::config::EmailConfig;
use crate::error::EmailError;

/// Default outgoing subject line. Subject customization (e.g. threading an
/// outgoing message onto an inbound one's `Subject:`/`In-Reply-To:`) is
/// Phase 2 — [`OutgoingMessage`] carries no subject field yet.
const DEFAULT_SUBJECT: &str = "Message from Ardur";

/// An email channel adapter: sends plaintext mail via SMTP and forwards
/// unseen inbox mail through the gateway via an IMAP poll loop.
///
/// Construct with [`EmailChannel::new`] (validates the IMAP + SMTP
/// credentials by connecting once), then call [`start`](Self::start) once to
/// begin draining inbound traffic. Hold it behind `dyn MessagingGateway` to
/// send and [`receive`](MessagingGateway::receive).
pub struct EmailChannel {
    config: EmailConfig,
    smtp: AsyncSmtpTransport<Tokio1Executor>,
    channel_id: ChannelId,
    inbound_tx: mpsc::UnboundedSender<IncomingMessage>,
    /// The drain side of the inbound queue. `receive(&self)` needs `&mut`
    /// access to `recv`, so a Mutex hands out exclusive access behind the
    /// shared ref.
    inbound_rx: Mutex<mpsc::UnboundedReceiver<IncomingMessage>>,
    /// Set once [`start`](Self::start) has spawned the poll loop, so a second
    /// call is a no-op rather than a second competing poller.
    started: AtomicBool,
}

impl EmailChannel {
    /// Build the SMTP transport and validate both credentials (an IMAP
    /// connect + login + logout round trip, run on a blocking task, plus
    /// eager construction of the SMTP transport — the transport itself
    /// connects lazily on first send).
    ///
    /// This does **not** start polling — call [`start`](Self::start)
    /// afterwards.
    ///
    /// # Errors
    /// [`EmailError::ImapConnect`] if the IMAP login round trip fails.
    /// [`EmailError::SmtpSend`] if the SMTP transport cannot be built.
    pub async fn new(config: EmailConfig) -> Result<Self, EmailError> {
        verify_imap_login(&config).await?;

        let creds = Credentials::new(
            config.address.clone(),
            config.password.expose_secret().to_owned(),
        );
        let smtp = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.smtp_host)
            .map_err(|e| EmailError::SmtpSend(e.to_string()))?
            .port(config.smtp_port)
            .credentials(creds)
            .build();

        let (tx, rx) = mpsc::unbounded_channel();
        let channel_id = ChannelId(format!("email://{}", config.address));

        Ok(Self {
            config,
            smtp,
            channel_id,
            inbound_tx: tx,
            inbound_rx: Mutex::new(rx),
            started: AtomicBool::new(false),
        })
    }

    /// Spawn the blocking IMAP poll loop.
    ///
    /// Idempotency is enforced: the first call spawns the loop; a second call
    /// is a logged no-op. The spawned task runs until the process exits,
    /// retrying (with the same poll interval as a backoff) past transient
    /// connect/fetch errors rather than exiting the loop.
    pub fn start(&self) {
        if self.started.swap(true, Ordering::SeqCst) {
            tracing::warn!("email channel already started; ignoring the second start");
            return;
        }

        let config = self.config.clone();
        let tx = self.inbound_tx.clone();
        let channel_prefix = self.channel_id.0.clone();

        tokio::task::spawn_blocking(move || poll_loop(&config, &tx, &channel_prefix));
    }

    /// Resolve an [`OutgoingMessage`] target into a recipient address.
    fn target_address(target: &MessageTarget) -> Result<String, EmailError> {
        match target {
            MessageTarget::User(u) => Ok(u.0.clone()),
            MessageTarget::Channel(_) => Err(EmailError::UnsupportedTarget(
                "email adapter Phase 1 addresses a single recipient, not a broadcast channel"
                    .to_owned(),
            )),
            MessageTarget::Thread(_) => Err(EmailError::UnsupportedTarget(
                "email adapter Phase 1 cannot deliver into a thread".to_owned(),
            )),
        }
    }

    /// Render an [`OutgoingMessage`] body into the plaintext a mail body
    /// carries.
    fn body_text(body: &MessageBody) -> Result<String, EmailError> {
        match body {
            MessageBody::Text(t) | MessageBody::Markdown(t) => Ok(t.clone()),
            MessageBody::Mention { user_ref, body } => Ok(format!("{} {}", user_ref.0, body)),
            MessageBody::Attachment { .. } => Err(EmailError::UnsupportedTarget(
                "email adapter Phase 1 cannot send attachments".to_owned(),
            )),
        }
    }

    /// Send plaintext to a recipient address, returning a synthetic message
    /// id (the `imap`/`lettre` crates do not surface a provider-assigned
    /// Message-ID from the SMTP response).
    ///
    /// The adapter's native send — preserves the full [`EmailError`]
    /// taxonomy the trait lowers onto the coarser [`GatewayError`].
    ///
    /// # Errors
    /// - [`EmailError::MessageBuild`] if `to_address` does not parse or the
    ///   message cannot be built.
    /// - [`EmailError::SmtpSend`] if the SMTP server rejects the send.
    pub async fn send_text(&self, to_address: &str, text: &str) -> Result<String, EmailError> {
        let from: Mailbox = self
            .config
            .address
            .parse()
            .map_err(|e: lettre::address::AddressError| EmailError::MessageBuild(e.to_string()))?;
        let recipient: Mailbox = to_address
            .parse()
            .map_err(|e: lettre::address::AddressError| EmailError::MessageBuild(e.to_string()))?;

        let email = LettreMessage::builder()
            .from(from)
            .to(recipient)
            .subject(DEFAULT_SUBJECT)
            .body(text.to_owned())
            .map_err(|e| EmailError::MessageBuild(e.to_string()))?;

        self.smtp
            .send(email)
            .await
            .map_err(|e| EmailError::SmtpSend(e.to_string()))?;

        Ok(Uuid::new_v4().to_string())
    }

    /// Email has no in-place message edit, so Phase 1 "edits" by sending a
    /// fresh message carrying the updated text (ignoring the previous
    /// message id) and returns a new synthetic id.
    ///
    /// Callers should prefer sending once at completion rather than editing
    /// per streamed delta for this channel — see the server's
    /// `Origin::Email` handling, which withholds delivery until the turn
    /// finishes rather than emailing every intermediate chunk.
    ///
    /// # Errors
    /// As [`send_text`](Self::send_text).
    pub async fn edit_text(
        &self,
        to_address: &str,
        _message_id: &str,
        text: &str,
    ) -> Result<String, EmailError> {
        self.send_text(to_address, text).await
    }
}

#[async_trait]
impl MessagingGateway for EmailChannel {
    async fn send_message(&self, msg: OutgoingMessage) -> Result<MessageReceipt, GatewayError> {
        let to = Self::target_address(&msg.target).map_err(|e| e.into_gateway_error(""))?;
        let body = Self::body_text(&msg.body).map_err(|e| e.into_gateway_error(&to))?;

        let message_id = self
            .send_text(&to, &body)
            .await
            .map_err(|e| e.into_gateway_error(&to))?;

        Ok(MessageReceipt {
            delivered_to: msg.channel_id,
            delivered_at: now_millis(),
            provider_message_id: Some(message_id),
            receipt_id: msg.message_id,
        })
    }

    async fn receive(&self) -> Result<IncomingMessage, GatewayError> {
        let mut rx = self.inbound_rx.lock().await;
        // `None` only if every sender dropped; `self` holds a clone of `tx`,
        // so this cannot happen while `self` is alive.
        rx.recv()
            .await
            .ok_or_else(|| GatewayError::DeliveryFailed("email inbound channel closed".to_owned()))
    }

    fn channel_id(&self) -> ChannelId {
        self.channel_id.clone()
    }

    fn supports_threading(&self) -> bool {
        // Reply-chain threading via In-Reply-To/References is Phase 2.
        false
    }
}

/// Connect, log in, and log out once — a synchronous round trip run on a
/// blocking task, used by [`EmailChannel::new`] to validate the IMAP
/// credentials eagerly rather than only discovering a bad password once the
/// poll loop is already running.
async fn verify_imap_login(config: &EmailConfig) -> Result<(), EmailError> {
    let config = config.clone();
    tokio::task::spawn_blocking(move || {
        let client = imap::ClientBuilder::new(&config.imap_host, config.imap_port)
            .connect()
            .map_err(|e| EmailError::ImapConnect(e.to_string()))?;
        let mut session = client
            .login(&config.address, config.password.expose_secret())
            .map_err(|(e, _client)| EmailError::ImapConnect(e.to_string()))?;
        session
            .logout()
            .map_err(|e| EmailError::ImapConnect(e.to_string()))?;
        Ok(())
    })
    .await
    .map_err(|e| EmailError::ImapConnect(e.to_string()))?
}

/// The blocking poll loop body: connect, select `INBOX`, then loop
/// fetch-forward-mark-seen-sleep. Reconnects (after sleeping one interval) on
/// any connect/select failure rather than exiting, so a transient network
/// blip does not permanently kill inbound delivery.
fn poll_loop(
    config: &EmailConfig,
    tx: &mpsc::UnboundedSender<IncomingMessage>,
    channel_prefix: &str,
) {
    let interval = Duration::from_secs(config.poll_interval_secs.max(1));
    loop {
        match connect_and_select(config) {
            Ok(mut session) => loop {
                if let Err(e) = poll_once(&mut session, config, tx, channel_prefix) {
                    tracing::warn!(error = %e, "email poll iteration failed; reconnecting");
                    break;
                }
                std::thread::sleep(interval);
            },
            Err(e) => {
                tracing::warn!(error = %e, "email imap connect failed; retrying after interval");
            }
        }
        std::thread::sleep(interval);
    }
}

/// The IMAP session type this crate polls: a `Session` over the TLS
/// connection type [`imap::ClientBuilder::connect`] returns.
type ImapSession = imap::Session<imap::Connection>;

/// Connect, log in, and `SELECT INBOX`.
fn connect_and_select(config: &EmailConfig) -> Result<ImapSession, EmailError> {
    let client = imap::ClientBuilder::new(&config.imap_host, config.imap_port)
        .connect()
        .map_err(|e| EmailError::ImapConnect(e.to_string()))?;
    let mut session = client
        .login(&config.address, config.password.expose_secret())
        .map_err(|(e, _client)| EmailError::ImapConnect(e.to_string()))?;
    session
        .select("INBOX")
        .map_err(|e| EmailError::ImapOperation(e.to_string()))?;
    Ok(session)
}

/// One fetch-forward-mark-seen cycle: search `UNSEEN`, fetch each, parse,
/// gate, forward, and mark `\Seen`.
fn poll_once(
    session: &mut ImapSession,
    config: &EmailConfig,
    tx: &mpsc::UnboundedSender<IncomingMessage>,
    channel_prefix: &str,
) -> Result<(), EmailError> {
    let unseen = session
        .uid_search("UNSEEN")
        .map_err(|e| EmailError::ImapOperation(e.to_string()))?;
    if unseen.is_empty() {
        return Ok(());
    }

    let uid_set = unseen
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let fetches = session
        .uid_fetch(&uid_set, "RFC822")
        .map_err(|e| EmailError::ImapOperation(e.to_string()))?;

    for fetch in fetches.iter() {
        let Some(uid) = fetch.uid else { continue };
        let Some(raw) = fetch.body() else { continue };

        match mail_parser::MessageParser::default().parse(raw) {
            Some(parsed) => {
                let from = parsed
                    .from()
                    .and_then(|f| f.first())
                    .and_then(|a| a.address())
                    .unwrap_or("unknown")
                    .to_ascii_lowercase();

                // Echo prevention: never re-ingest mail the account itself sent.
                if from.eq_ignore_ascii_case(&config.address) {
                    continue;
                }
                if !config.sender_allowed(&from) {
                    tracing::warn!(
                        sender = %from,
                        "dropping email from a sender outside the allowlist"
                    );
                    continue;
                }

                let body = parsed
                    .body_text(0)
                    .map(|c| c.into_owned())
                    .unwrap_or_default();
                let incoming = IncomingMessage {
                    message_id: Uuid::new_v4(),
                    // Embed the sender in the channel id (matching the
                    // Discord/Telegram `<prefix>/<chat-id>` shape) so the
                    // reply routes back to the address that wrote in, not to
                    // this account's own address.
                    channel_id: ChannelId(format!("{channel_prefix}/{from}")),
                    sender: SenderRef(from),
                    body: MessageBody::Text(body),
                    received_at: now_millis(),
                    thread_id: None,
                };
                if tx.send(incoming).is_err() {
                    tracing::error!("email inbound receiver is gone; dropping message");
                }
            }
            None => {
                tracing::warn!(uid = uid, "failed to parse fetched email; skipping");
            }
        }

        session
            .uid_store(uid.to_string(), "+FLAGS (\\Seen)")
            .map_err(|e| EmailError::ImapOperation(e.to_string()))?;
    }

    Ok(())
}

/// Current wall-clock time in Unix milliseconds (saturating to 0 before the
/// epoch).
fn now_millis() -> UnixTsMillis {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_address_rejects_channel_and_thread() {
        use ardur_messaging_gateway::{ChannelRef, ThreadRef, UserRef};

        assert!(
            EmailChannel::target_address(&MessageTarget::User(UserRef("a@example.com".to_owned())))
                .is_ok()
        );
        assert!(matches!(
            EmailChannel::target_address(&MessageTarget::Channel(ChannelRef("c".to_owned()))),
            Err(EmailError::UnsupportedTarget(_))
        ));
        assert!(matches!(
            EmailChannel::target_address(&MessageTarget::Thread(ThreadRef("t".to_owned()))),
            Err(EmailError::UnsupportedTarget(_))
        ));
    }

    #[test]
    fn body_text_rejects_attachments() {
        assert_eq!(
            EmailChannel::body_text(&MessageBody::Text("hi".to_owned())).unwrap(),
            "hi"
        );
        assert!(matches!(
            EmailChannel::body_text(&MessageBody::Attachment {
                name: "f.txt".to_owned(),
                mime_type: "text/plain".to_owned(),
                bytes: vec![],
            }),
            Err(EmailError::UnsupportedTarget(_))
        ));
    }
}
