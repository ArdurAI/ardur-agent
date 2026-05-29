//! The crate's typed error surfaces: [`GatewayError`] for delivery/receive and
//! [`RegistryError`] for registration.

use ardur_runtime::CapTokenRef;

use crate::types::ChannelId;

/// A failure delivering or receiving a message through a gateway.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// No gateway is registered for the requested channel.
    #[error("no gateway registered for channel {0:?}")]
    ChannelNotFound(ChannelId),
    /// The capability token authorizing the send was rejected.
    // TODO §4.0 Phase 2: verify against the §11.14 cap-token verifier rather
    // than treating every token as valid in-process.
    #[error("capability token rejected: {0:?}")]
    CapTokenInvalid(CapTokenRef),
    /// The channel's send rate limit was exceeded.
    #[error("channel is rate limited")]
    RateLimited,
    /// The backend accepted but failed to deliver the message.
    #[error("delivery failed: {0}")]
    DeliveryFailed(String),
    /// The message requested a feature the channel does not support (e.g.
    /// threaded delivery on a channel where [`supports_threading`] is false).
    ///
    /// [`supports_threading`]: crate::MessagingGateway::supports_threading
    #[error("unsupported feature: {0}")]
    UnsupportedFeature(String),
    /// An unexpected internal error.
    #[error("internal gateway error: {0}")]
    Internal(#[from] anyhow::Error),
}

/// A failure registering a gateway with the [`GatewayRegistry`].
///
/// [`GatewayRegistry`]: crate::GatewayRegistry
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// A gateway is already registered for this channel id.
    #[error("a gateway is already registered for channel {0:?}")]
    AlreadyRegistered(ChannelId),
}
