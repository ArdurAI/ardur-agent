//! The [`Provider`] trait — the uniform interface every model backend
//! (Anthropic, OpenAI, Ollama, …) presents to the runtime.

use ardur_runtime::ProviderId;
use async_trait::async_trait;

use crate::error::ProviderError;
use crate::rate_card::RateCard;
use crate::types::{CompletionRequest, CompletionResponse};

/// A model backend the runtime can dispatch completions to.
///
/// The trait is object-safe (via `async-trait`), so providers are stored and
/// dispatched as `dyn Provider` through the
/// [`ProviderRegistry`](crate::ProviderRegistry). Implementors must be `Send +
/// Sync` to be shared across the async runtime.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Run one completion, returning the generated response or a typed failure.
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError>;

    /// The registry key this provider answers to (e.g. `"anthropic"`).
    fn id(&self) -> ProviderId;

    /// Whether this provider can stream tokens incrementally.
    fn supports_streaming(&self) -> bool;

    /// The pricing table the provider's costs are computed under.
    fn rate_card(&self) -> &RateCard;
}
