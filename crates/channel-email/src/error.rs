//! [`EmailError`] — the adapter's typed configuration / connect / send /
//! parse failure surface, lowered onto [`GatewayError`] at the
//! [`MessagingGateway`] boundary.
//!
//! [`GatewayError`]: ardur_messaging_gateway::GatewayError
//! [`MessagingGateway`]: ardur_messaging_gateway::MessagingGateway

use ardur_messaging_gateway::{ChannelId, GatewayError};

/// A failure configuring, connecting, sending through, or receiving from the
/// email adapter.
#[derive(Debug, thiserror::Error)]
pub enum EmailError {
    /// A required environment variable was unset or empty when building from
    /// the environment. Carries the variable name.
    #[error("required environment variable {0} is not set")]
    MissingEnvVar(String),
    /// A required configuration field was empty when building from the
    /// builder.
    #[error("required configuration field {0} is empty")]
    MissingField(String),
    /// An `ARDUR_EMAIL_ALLOWED_SENDERS` entry was not a plausible address
    /// (must contain `@`). Carries the entry.
    #[error("invalid allowed-sender address: {0}")]
    InvalidSenderAddress(String),
    /// A configured port string failed to parse as `u16`.
    #[error("invalid port: {0}")]
    InvalidPort(String),
    /// Connecting to the IMAP server failed (TLS handshake, DNS, login).
    #[error("failed to connect the imap client: {0}")]
    ImapConnect(String),
    /// An IMAP command (select / search / fetch / store) failed after a
    /// connection was established.
    #[error("imap operation failed: {0}")]
    ImapOperation(String),
    /// Building or sending the SMTP transport failed.
    #[error("failed to send via smtp: {0}")]
    SmtpSend(String),
    /// Building the outgoing message (invalid address, malformed body)
    /// failed.
    #[error("failed to build outgoing email: {0}")]
    MessageBuild(String),
    /// Parsing a fetched IMAP message into headers/body failed.
    #[error("failed to parse fetched email: {0}")]
    Parse(String),
    /// The send addressed a target shape Phase 1 does not implement (a
    /// channel/thread target — email only addresses users by address).
    /// Carries a human description.
    #[error("unsupported email target: {0}")]
    UnsupportedTarget(String),
}

impl EmailError {
    /// Lower an email-specific failure onto the §4.0 [`GatewayError`] the
    /// [`MessagingGateway`] contract speaks in.
    ///
    /// `InvalidSenderAddress` / `InvalidPort` carry the namespaced channel id
    /// into [`GatewayError::ChannelNotFound`]; `UnsupportedTarget` maps to
    /// [`GatewayError::UnsupportedFeature`]; `SmtpSend` / `ImapConnect` /
    /// `ImapOperation` / `MessageBuild` collapse to
    /// [`GatewayError::DeliveryFailed`] (preserving the [`Display`] text); the
    /// remaining config-time variants lower to [`GatewayError::Internal`].
    ///
    /// `target` is the address string the send was addressed to, surfaced in
    /// the [`GatewayError::ChannelNotFound`] case.
    ///
    /// [`GatewayError`]: ardur_messaging_gateway::GatewayError
    /// [`MessagingGateway`]: ardur_messaging_gateway::MessagingGateway
    /// [`Display`]: std::fmt::Display
    #[must_use]
    pub fn into_gateway_error(self, target: &str) -> GatewayError {
        match self {
            EmailError::InvalidSenderAddress(_) | EmailError::InvalidPort(_) => {
                GatewayError::ChannelNotFound(ChannelId(target.to_owned()))
            }
            EmailError::UnsupportedTarget(msg) => GatewayError::UnsupportedFeature(msg),
            other @ (EmailError::SmtpSend(_)
            | EmailError::ImapConnect(_)
            | EmailError::ImapOperation(_)
            | EmailError::MessageBuild(_)) => GatewayError::DeliveryFailed(other.to_string()),
            other => GatewayError::Internal(anyhow::Error::msg(other.to_string())),
        }
    }
}
