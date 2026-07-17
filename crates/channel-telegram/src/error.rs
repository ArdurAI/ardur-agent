//! [`TelegramError`] — the adapter's typed configuration / connect / send
//! failure surface, lowered onto [`GatewayError`] at the [`MessagingGateway`]
//! boundary.
//!
//! [`GatewayError`]: ardur_messaging_gateway::GatewayError
//! [`MessagingGateway`]: ardur_messaging_gateway::MessagingGateway

use ardur_messaging_gateway::{ChannelId, GatewayError};

/// A failure configuring, connecting, sending through, or receiving from the
/// Telegram adapter.
#[derive(Debug, thiserror::Error)]
pub enum TelegramError {
    /// A required environment variable was unset or empty when building from the
    /// environment. Carries the variable name.
    #[error("required environment variable {0} is not set")]
    MissingEnvVar(String),
    /// A required configuration field was empty when building from the builder.
    #[error("required configuration field {0} is empty")]
    MissingField(String),
    /// A `TELEGRAM_ALLOWED_CHATS` entry was not an `i64`. Carries the entry.
    #[error("invalid telegram chat id: {0}")]
    InvalidChatId(String),
    /// Connecting to the Bot API failed — typically `get_me` rejecting the token.
    #[error("failed to connect the telegram bot: {0}")]
    Connect(String),
    /// Sending the message to Telegram failed (the bot is not a member of the
    /// chat, blocked, rate-limited, …).
    #[error("failed to send to telegram: {0}")]
    Send(String),
    /// The send addressed a target shape Phase 1 does not implement (a direct
    /// message to a user handle, or a threaded reply). Carries a human
    /// description.
    #[error("unsupported telegram target: {0}")]
    UnsupportedTarget(String),
}

impl TelegramError {
    /// Lower a Telegram-specific failure onto the §4.0 [`GatewayError`] the
    /// [`MessagingGateway`] contract speaks in.
    ///
    /// `InvalidChatId` carries the namespaced channel id into
    /// [`GatewayError::ChannelNotFound`]; `UnsupportedTarget` maps to
    /// [`GatewayError::UnsupportedFeature`]; `Send` / `Connect` collapse to
    /// [`GatewayError::DeliveryFailed`] (preserving the [`Display`] text); the
    /// remaining config-time variants lower to [`GatewayError::Internal`].
    ///
    /// `target` is the chat id string the send was addressed to, surfaced in the
    /// [`GatewayError::ChannelNotFound`] case.
    ///
    /// [`GatewayError`]: ardur_messaging_gateway::GatewayError
    /// [`MessagingGateway`]: ardur_messaging_gateway::MessagingGateway
    /// [`Display`]: std::fmt::Display
    #[must_use]
    pub fn into_gateway_error(self, target: &str) -> GatewayError {
        match self {
            TelegramError::InvalidChatId(_) => {
                GatewayError::ChannelNotFound(ChannelId(target.to_owned()))
            }
            TelegramError::UnsupportedTarget(msg) => GatewayError::UnsupportedFeature(msg),
            other @ (TelegramError::Send(_) | TelegramError::Connect(_)) => {
                GatewayError::DeliveryFailed(other.to_string())
            }
            other => GatewayError::Internal(anyhow::Error::msg(other.to_string())),
        }
    }
}
