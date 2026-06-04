//! [`MatrixError`] — the adapter's typed configuration / connect / send failure
//! surface, lowered onto [`GatewayError`] at the [`MessagingGateway`] boundary.
//!
//! [`GatewayError`]: ardur_messaging_gateway::GatewayError
//! [`MessagingGateway`]: ardur_messaging_gateway::MessagingGateway

use ardur_messaging_gateway::{ChannelId, GatewayError};

/// A failure configuring, connecting, sending through, or receiving from the
/// Matrix adapter.
#[derive(Debug, thiserror::Error)]
pub enum MatrixError {
    /// A required environment variable was unset or empty when building from the
    /// environment. Carries the variable name.
    #[error("required environment variable {0} is not set")]
    MissingEnvVar(String),
    /// A required configuration field was empty when building from the builder.
    #[error("required configuration field {0} is empty")]
    MissingField(String),
    /// The configured `user_id` is not a well-formed Matrix user id
    /// (`@name:homeserver`). Carries the offending value.
    #[error("invalid matrix user id: {0}")]
    InvalidUserId(String),
    /// The target room id is not a well-formed Matrix room id (`!id:homeserver`).
    /// Carries the offending value.
    #[error("invalid matrix room id: {0}")]
    InvalidRoomId(String),
    /// Building the `matrix_sdk::Client` or restoring the bot session failed
    /// (bad homeserver URL, unreachable server, rejected access token, …).
    #[error("failed to connect the matrix client: {0}")]
    Connect(String),
    /// The bot is not joined to the target room (so it cannot post there).
    #[error("the bot is not in matrix room {0}")]
    RoomNotFound(String),
    /// Sending the message to the homeserver failed.
    #[error("failed to send to matrix: {0}")]
    Send(String),
    /// The send addressed a target shape Phase 1 does not implement (a direct
    /// message to a user, or a threaded reply). Carries a human description.
    #[error("unsupported matrix target: {0}")]
    UnsupportedTarget(String),
}

impl MatrixError {
    /// Lower a Matrix-specific failure onto the §4.0 [`GatewayError`] the
    /// [`MessagingGateway`] contract speaks in.
    ///
    /// `RoomNotFound` carries the namespaced channel id into
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
            MatrixError::RoomNotFound(_) => {
                GatewayError::ChannelNotFound(ChannelId(target.to_owned()))
            }
            MatrixError::UnsupportedTarget(msg) => GatewayError::UnsupportedFeature(msg),
            other @ (MatrixError::Send(_) | MatrixError::Connect(_)) => {
                GatewayError::DeliveryFailed(other.to_string())
            }
            other => GatewayError::Internal(anyhow_msg(other.to_string())),
        }
    }
}

/// Build a `GatewayError::Internal` payload without taking an `anyhow` dependency
/// — the gateway's `Internal(#[from] anyhow::Error)` accepts any `Error`, and a
/// boxed string error is the smallest carrier of the message text.
fn anyhow_msg(msg: String) -> anyhow::Error {
    anyhow::Error::msg(msg)
}
