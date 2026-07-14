//! [`PwaError`] — the crate's typed failure surface.

/// A failure generating/loading the VAPID key, registering a subscription,
/// or sending a push.
#[derive(Debug, thiserror::Error)]
pub enum PwaError {
    /// The VAPID private key file exists but failed to parse.
    #[error("failed to load the vapid key: {0}")]
    KeyLoad(String),
    /// Writing the VAPID key or the subscription store to disk failed.
    #[error("failed to persist pwa state: {0}")]
    Persist(String),
    /// A subscription's `endpoint`/`p256dh`/`auth` fields failed validation
    /// (per the OpenClaw precedent this crate adapts: endpoint must be
    /// `https://`, at most 2048 chars).
    #[error("invalid push subscription: {0}")]
    InvalidSubscription(String),
    /// No subscription is registered under the requested id.
    #[error("no subscription registered with id {0}")]
    UnknownSubscription(uuid::Uuid),
    /// Building the VAPID signature or encrypting the push payload failed.
    #[error("failed to build the push message: {0}")]
    MessageBuild(String),
    /// The push service rejected the delivery (non-2xx response).
    #[error("push service rejected the delivery: {0}")]
    DeliveryFailed(String),
}
