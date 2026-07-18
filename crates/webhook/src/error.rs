use thiserror::Error;

/// Typed errors for the webhook crate.
#[derive(Debug, Error)]
pub enum WebhookError {
    /// Signature verification failed (bad secret, tampered body, or malformed header).
    #[error("signature verification failed")]
    SignatureVerificationFailed,
    /// The webhook payload could not be parsed.
    #[error("payload parse failed: {0}")]
    PayloadParseFailed(String),
    /// The configured endpoint is invalid.
    #[error("invalid endpoint: {0}")]
    InvalidEndpoint(String),
    /// An outbound request failed after retries.
    #[error("outbound request failed: {0}")]
    OutboundRequestFailed(String),
    /// The requested endpoint or handler was not found.
    #[error("handler not found: {0}")]
    HandlerNotFound(String),
    /// An operator action was refused: the cap-token lacked the required scope
    /// or the caller does not own the target resource (§9.7).
    #[error("action refused: {0}")]
    Denied(String),
    /// Cap-token verification failed (expired, wrong audience, malformed).
    #[error("cap-token: {0}")]
    CapToken(String),
    /// Receipt signing/persistence failed.
    #[error("receipt: {0}")]
    Receipt(String),
    /// A persistence (read/write) failure in a durable store.
    #[error("store i/o: {0}")]
    Io(String),
    /// A (de)serialization failure of a stored record.
    #[error("serde: {0}")]
    Serde(String),
    /// The HMAC signing secret could not be resolved from its environment ref.
    #[error("signing key resolve failed: {0}")]
    SigningKeyResolveFailed(String),
    /// Generic internal error (opaque).
    #[error("internal error: {0}")]
    Internal(String),
}

/// Convenience type alias for webhook operations.
pub type Result<T> = std::result::Result<T, WebhookError>;
