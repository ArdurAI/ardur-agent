//! [`EmbeddingProvider`] — the text-embedding sibling of [`Provider`](crate::Provider) (§3.4).
//!
//! Kept as a separate object-safe trait rather than a method on `Provider`:
//! completion and embedding are different capabilities with different
//! request/response shapes, and not every backend offers both (Anthropic has
//! no embeddings endpoint; OpenAI-compatible and Azure OpenAI endpoints do). A
//! completion-only backend needs no embeddings stub this way.
//!
//! This is distinct from `ardur-embeddings` (§7.2), which is a local-only ONNX
//! embedder for the memory/RAG dense-search path — unrelated to the
//! model-provider layer this trait extends.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ProviderError;
use crate::rate_card::RateCard;
use ardur_runtime::ProviderId;

/// One embedding request: a batch of input strings against a named model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    /// The texts to embed, in order. The response's `vectors` are returned in
    /// the same order.
    pub input: Vec<String>,
    /// The embedding model to run against (opaque to this layer; the backend
    /// validates it).
    pub model: String,
}

impl EmbeddingRequest {
    /// Build a request embedding `input` against `model`.
    pub fn new(input: Vec<String>, model: impl Into<String>) -> Self {
        Self {
            input,
            model: model.into(),
        }
    }
}

/// The result of one [`EmbeddingProvider::embed`] call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingResponse {
    /// One vector per input string, same order as
    /// [`EmbeddingRequest::input`].
    pub vectors: Vec<Vec<f32>>,
    /// Raw token counts the provider billed (`tokens_out` is always `0` — an
    /// embedding call has no generated tokens).
    pub usage: crate::types::Usage,
    /// The untouched upstream response body, when retained for audit.
    pub raw_provider_response: Option<serde_json::Value>,
}

/// A model backend the runtime can dispatch text-embedding requests to.
///
/// Object-safe (via `async-trait`) so implementors are stored and dispatched
/// as `dyn EmbeddingProvider` through the [`EmbeddingProviderRegistry`].
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed a batch of input strings, returning one vector per input.
    async fn embed(&self, req: EmbeddingRequest) -> Result<EmbeddingResponse, ProviderError>;

    /// The registry key this provider answers to (e.g. `"openai-compat"`).
    fn id(&self) -> ProviderId;

    /// The dimension of every vector this provider's default model produces.
    fn embedding_dim(&self) -> usize;

    /// The pricing table the provider's embedding costs are computed under.
    fn rate_card(&self) -> &RateCard;
}
