//! [`AnthropicProvider`] — the Anthropic backend. Phase 1 is a stub: it holds
//! the credentials and pricing but returns a fixed placeholder completion
//! instead of issuing a network call.

use ardur_runtime::{CostTuple, ProviderId};
use async_trait::async_trait;

use crate::error::ProviderError;
use crate::provider::Provider;
use crate::rate_card::RateCard;
use crate::types::{CompletionRequest, CompletionResponse, FinishReason, ModelId, Usage};

/// The Anthropic provider.
///
/// Phase 1 ships the contract surface and credential/pricing custody only;
/// [`AnthropicProvider::complete`] returns a deterministic stub. The live
/// Messages-API HTTP path arrives in Phase 2.
pub struct AnthropicProvider {
    api_key: String,
    model_id: ModelId,
    rate_card: RateCard,
}

impl AnthropicProvider {
    /// Construct a provider bound to `api_key` and a default `model_id`, priced
    /// by [`RateCard::anthropic_2026_q2_v1`].
    pub fn new(api_key: impl Into<String>, model_id: ModelId) -> Self {
        Self {
            api_key: api_key.into(),
            model_id,
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }

    /// The model this provider defaults completions to.
    #[must_use]
    pub fn model_id(&self) -> &ModelId {
        &self.model_id
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    // TODO §3.0 Phase 2: replace the stub with a real Anthropic Messages-API
    // call — serialize `req`, POST it, map the response onto CompletionResponse,
    // and price `usage` through `self.rate_card`. See
    // `plans/3.1-anthropic-provider-blueprint.md`.
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        if self.api_key.is_empty() {
            return Err(ProviderError::Unauthorized);
        }
        Ok(CompletionResponse {
            content: "[anthropic stub]".to_string(),
            finish_reason: FinishReason::Stop,
            usage: Usage {
                tokens_in: 0,
                tokens_out: 0,
            },
            cost: CostTuple::default(),
            raw_provider_response: None,
        })
    }

    fn id(&self) -> ProviderId {
        ProviderId("anthropic".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}
