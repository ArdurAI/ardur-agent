//! [`SlackError`] — the adapter's typed failure surface.
//!
//! It is a *superset* of the seven variants the §4.1 task enumerates
//! ([`InvalidSignature`](SlackError::InvalidSignature),
//! [`Replay`](SlackError::Replay), [`MissingEnvVar`](SlackError::MissingEnvVar),
//! [`Upstream`](SlackError::Upstream),
//! [`NetworkFailure`](SlackError::NetworkFailure),
//! [`ParseError`](SlackError::ParseError),
//! [`UnsupportedEvent`](SlackError::UnsupportedEvent)). The send path needs to
//! preserve the distinct Slack API failures the task spells out
//! (`not_in_channel` / `invalid_auth` / `channel_not_found` / `ratelimited`)
//! losslessly *before* they are lowered onto the coarser [`GatewayError`]
//! variants at the [`MessagingGateway`] boundary — so
//! [`Forbidden`](SlackError::Forbidden), [`Unauthorized`](SlackError::Unauthorized),
//! [`ChannelNotFound`](SlackError::ChannelNotFound), and
//! [`RateLimited`](SlackError::RateLimited) join them here.
//!
//! [`GatewayError`]: ardur_messaging_gateway::GatewayError
//! [`MessagingGateway`]: ardur_messaging_gateway::MessagingGateway

use ardur_messaging_gateway::{ChannelId, GatewayError};

/// A failure sending to, receiving from, or verifying a request against the
/// Slack adapter.
#[derive(Debug, thiserror::Error)]
pub enum SlackError {
    /// The presented `X-Slack-Signature` did not match the HMAC computed over
    /// the request basestring with the signing secret.
    #[error("slack request signature is invalid")]
    InvalidSignature,
    /// The request's `X-Slack-Request-Timestamp` is outside the ±5-minute replay
    /// window. Carries the request's age in seconds (absolute skew from now).
    #[error("slack request is outside the replay window ({age_seconds}s skew)")]
    Replay {
        /// Absolute difference between now and the request timestamp, seconds.
        age_seconds: u64,
    },
    /// A required environment variable was unset when building from the
    /// environment. Carries the variable name.
    #[error("required environment variable {0} is not set")]
    MissingEnvVar(String),
    /// Slack rejected the send with `not_in_channel` — the bot is not a member
    /// of the target channel.
    #[error("slack rejected the send: the bot is not in the channel")]
    Forbidden,
    /// Slack rejected the send with `invalid_auth` — the bot token is missing,
    /// malformed, or revoked.
    #[error("slack rejected the bot token as invalid")]
    Unauthorized,
    /// Slack rejected the send with `channel_not_found` — the target channel
    /// does not exist or is not visible to the bot.
    #[error("slack could not find the target channel")]
    ChannelNotFound,
    /// Slack rate-limited the send (`ratelimited`, or HTTP 429). Carries the
    /// `Retry-After` delay in milliseconds (0 when the header was absent).
    #[error("slack rate-limited the send; retry after {retry_after_ms}ms")]
    RateLimited {
        /// Suggested backoff before retrying, milliseconds.
        retry_after_ms: u64,
    },
    /// Slack returned `ok: false` with an error code other than the ones mapped
    /// above. Carries the raw Slack error string.
    #[error("slack returned an upstream error: {0}")]
    Upstream(String),
    /// The HTTP request to Slack failed to complete (connection refused, TLS
    /// failure, timeout, …).
    #[error("network failure talking to slack: {0}")]
    NetworkFailure(String),
    /// A request or response body could not be parsed into the expected shape.
    #[error("failed to parse a slack payload: {0}")]
    ParseError(String),
    /// An inbound event arrived whose type the Phase-1 adapter does not handle
    /// (anything other than a `message` event or the `url_verification`
    /// handshake). Carries the unhandled event type.
    #[error("unsupported slack event type: {0}")]
    UnsupportedEvent(String),
}

impl SlackError {
    /// Lower a Slack-specific failure onto the §4.0 [`GatewayError`] the
    /// [`MessagingGateway`] contract speaks in.
    ///
    /// The mapping is intentionally lossy where `GatewayError` is coarser:
    /// `Forbidden` / `Unauthorized` / `Upstream` / `NetworkFailure` all collapse
    /// to [`GatewayError::DeliveryFailed`] (carrying the [`Display`] text so the
    /// distinction survives in the message), and [`RateLimited`] drops its
    /// `retry_after_ms` because [`GatewayError::RateLimited`] is fieldless. The
    /// inbound-only variants (`InvalidSignature`, `Replay`, `ParseError`,
    /// `UnsupportedEvent`, `MissingEnvVar`) are not reachable on the send path
    /// and lower to [`GatewayError::Internal`].
    ///
    /// `target` is the channel string the send was addressed to, surfaced in the
    /// [`GatewayError::ChannelNotFound`] case.
    ///
    /// [`GatewayError`]: ardur_messaging_gateway::GatewayError
    /// [`MessagingGateway`]: ardur_messaging_gateway::MessagingGateway
    /// [`RateLimited`]: SlackError::RateLimited
    /// [`Display`]: std::fmt::Display
    #[must_use]
    pub fn into_gateway_error(self, target: &str) -> GatewayError {
        match self {
            SlackError::ChannelNotFound => {
                GatewayError::ChannelNotFound(ChannelId(target.to_owned()))
            }
            SlackError::RateLimited { .. } => GatewayError::RateLimited,
            other @ (SlackError::Forbidden
            | SlackError::Unauthorized
            | SlackError::Upstream(_)
            | SlackError::NetworkFailure(_)) => GatewayError::DeliveryFailed(other.to_string()),
            other => GatewayError::Internal(anyhow::anyhow!(other.to_string())),
        }
    }
}
