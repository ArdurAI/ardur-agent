//! [`DiscordError`] — the adapter's typed configuration / connect / send failure
//! surface, lowered onto [`GatewayError`] at the [`MessagingGateway`] boundary.
//!
//! [`GatewayError`]: ardur_messaging_gateway::GatewayError
//! [`MessagingGateway`]: ardur_messaging_gateway::MessagingGateway

use ardur_messaging_gateway::{ChannelId, GatewayError};

/// A failure configuring, connecting, sending through, or receiving from the
/// Discord adapter.
#[derive(Debug, thiserror::Error)]
pub enum DiscordError {
    /// A required environment variable was unset or empty when building from the
    /// environment. Carries the variable name.
    #[error("required environment variable {0} is not set")]
    MissingEnvVar(String),
    /// A required configuration field was empty when building from the builder.
    #[error("required configuration field {0} is empty")]
    MissingField(String),
    /// `DISCORD_APPLICATION_ID` was not a `u64`. Carries the offending value.
    #[error("invalid discord application id: {0}")]
    InvalidApplicationId(String),
    /// A `DISCORD_ALLOWED_CHANNELS` entry was not a `u64`. Carries the entry.
    #[error("invalid discord channel id: {0}")]
    InvalidChannelId(String),
    /// Building the `serenity` client failed (bad token shape, …).
    #[error("failed to connect the discord client: {0}")]
    Connect(String),
    /// Sending the message to Discord failed (the bot lacks access to the
    /// channel, the channel does not exist, rate-limited, …).
    #[error("failed to send to discord: {0}")]
    Send(String),
    /// The send addressed a target shape Phase 1 does not implement (a direct
    /// message to a user id, or a threaded reply). Carries a human description.
    #[error("unsupported discord target: {0}")]
    UnsupportedTarget(String),
}

impl DiscordError {
    /// Lower a Discord-specific failure onto the §4.0 [`GatewayError`] the
    /// [`MessagingGateway`] contract speaks in.
    ///
    /// `InvalidChannelId` carries the namespaced channel id into
    /// [`GatewayError::ChannelNotFound`]; `UnsupportedTarget` maps to
    /// [`GatewayError::UnsupportedFeature`]; `Send` / `Connect` collapse to
    /// [`GatewayError::DeliveryFailed`] (preserving the [`Display`] text); the
    /// remaining config-time variants lower to [`GatewayError::Internal`] (they
    /// are not reachable on the send path).
    ///
    /// `target` is the channel id string the send was addressed to, surfaced in
    /// the [`GatewayError::ChannelNotFound`] case.
    ///
    /// [`GatewayError`]: ardur_messaging_gateway::GatewayError
    /// [`MessagingGateway`]: ardur_messaging_gateway::MessagingGateway
    /// [`Display`]: std::fmt::Display
    #[must_use]
    pub fn into_gateway_error(self, target: &str) -> GatewayError {
        match self {
            DiscordError::InvalidChannelId(_) => {
                GatewayError::ChannelNotFound(ChannelId(target.to_owned()))
            }
            DiscordError::UnsupportedTarget(msg) => GatewayError::UnsupportedFeature(msg),
            other @ (DiscordError::Send(_) | DiscordError::Connect(_)) => {
                GatewayError::DeliveryFailed(other.to_string())
            }
            other => GatewayError::Internal(anyhow::Error::msg(other.to_string())),
        }
    }
}
