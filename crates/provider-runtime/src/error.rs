//! The provider layer's single typed-error surface.

use crate::types::ModelId;

/// Every way a [`Provider::complete`](crate::Provider::complete) call can fail.
///
/// The variants name the *upstream-independent* failure classes the runtime's
/// admission/retry logic switches on; provider-specific detail (an HTTP body, a
/// vendor error code) is funnelled through [`ProviderError::Upstream`].
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The request never reached the provider (DNS, TCP, TLS, timeout).
    #[error("network failure reaching the provider")]
    NetworkFailure,

    /// The provider rejected the call for exceeding a rate limit. `retry_after_ms`
    /// is the back-off the caller should wait before retrying.
    #[error("rate limited; retry after {retry_after_ms} ms")]
    RateLimited {
        /// Milliseconds the caller should wait before retrying.
        retry_after_ms: u64,
    },

    /// The request was malformed or violated a provider constraint (bad
    /// parameter range, empty prompt, unsupported option).
    #[error("invalid completion request")]
    InvalidRequest,

    /// The requested model is unknown to, or not enabled for, this provider.
    #[error("model not available: {0}")]
    ModelNotAvailable(ModelId),

    /// Admitting the request would exceed its
    /// [`CostEnvelope`](crate::CostEnvelope) ceiling.
    #[error("requested completion exceeds its cost ceiling")]
    CostCeilingExceeded,

    /// Authentication failed — a missing, malformed, or revoked API key.
    #[error("provider rejected the credentials")]
    Unauthorized,

    /// A provider-specific failure not captured by the variants above; carries
    /// the upstream message verbatim.
    #[error("upstream provider error: {0}")]
    Upstream(String),
}
