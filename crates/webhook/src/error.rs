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
    /// Generic internal error (opaque).
    #[error("internal error: {0}")]
    Internal(String),
}

/// Convenience alias for [`std::result::Result`] with [`WebhookError`] as the
/// error type.
pub type Result<T> = std::result::Result<T, WebhookError>;
