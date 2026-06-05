//! The [`Provider`] trait — the uniform interface every model backend
//! (Anthropic, OpenAI, Ollama, …) presents to the runtime.

use ardur_runtime::ProviderId;
use async_trait::async_trait;

use crate::error::ProviderError;
use crate::rate_card::RateCard;
use crate::stream::{ProviderStream, events_from_response, iter_events};
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

    /// Stream a completion as a feed of [`StreamEvent`](crate::StreamEvent)s
    /// (§3.1b).
    ///
    /// The default implementation runs one [`complete`](Self::complete) and
    /// replays its result as a short, already-finished stream
    /// ([`events_from_response`]): the content delta, any tool-call starts, the
    /// final usage, then the terminal finish. This makes `stream()` available on
    /// every provider for free — a backend that can stream tokens incrementally
    /// (e.g. [`AnthropicProvider`](crate::AnthropicProvider)) overrides this with
    /// a real server-sent-event path, and the override is the only place that
    /// needs to change. Errors are surfaced two ways: a failure to *start* the
    /// stream is the `Err` of the returned `Result`, while a failure *mid-stream*
    /// is an `Err` item yielded by the stream itself.
    async fn stream(&self, req: CompletionRequest) -> Result<ProviderStream, ProviderError> {
        let response = self.complete(req).await?;
        Ok(iter_events(events_from_response(response)))
    }

    /// The registry key this provider answers to (e.g. `"anthropic"`).
    fn id(&self) -> ProviderId;

    /// A short, human-facing name for this provider — the value recorded in a
    /// receipt's `provider` field (§11.14b). Defaults to the registry
    /// [`id`](Self::id) string, which every provider already answers to, so
    /// existing backends need no change.
    fn name(&self) -> String {
        self.id().0
    }

    /// Whether this provider can stream tokens incrementally.
    fn supports_streaming(&self) -> bool;

    /// The pricing table the provider's costs are computed under.
    fn rate_card(&self) -> &RateCard;
}
