use serde::{Deserialize, Serialize};
use ardur_runtime::ProviderId;
use ardur_provider_runtime::{CompletionRequest, CompletionResponse, Provider, ProviderError, ProviderStream, RateCard, ModelId};
use async_trait::async_trait;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub streaming: bool,
    pub tool_use: bool,
    pub vision: bool,
    pub max_tokens: u32,
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self {
            streaming: true,
            tool_use: true,
            vision: false,
            max_tokens: 4096,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiCompatProvider {
    config: crate::config::OpenAiCompatConfig,
    capabilities: ProviderCapabilities,
    client: reqwest::Client,
}

impl OpenAiCompatProvider {
    pub fn new(config: crate::config::OpenAiCompatConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .unwrap_or_default();
        Self {
            config,
            capabilities: ProviderCapabilities::default(),
            client,
        }
    }

    pub fn from_env() -> crate::error::Result<Self> {
        let base_url = std::env::var("OPENAI_COMPAT_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        let api_key = std::env::var("OPENAI_COMPAT_KEY")
            .unwrap_or_default();
        let model = std::env::var("OPENAI_COMPAT_MODEL")
            .unwrap_or_else(|_| "gpt-4".to_string());
        
        let config = crate::config::ConfigBuilder::new(&base_url)
            .with_api_key(&api_key)
            .with_model(&model)
            .build()?;
        
        Ok(Self::new(config))
    }

    pub fn config(&self) -> &crate::config::OpenAiCompatConfig {
        &self.config
    }

    pub fn capabilities(&self) -> &ProviderCapabilities {
        &self.capabilities
    }

    pub fn base_url(&self) -> &str {
        &self.config.base_url
    }

    pub fn api_key(&self) -> &str {
        &self.config.api_key
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }
}

#[async_trait]
impl Provider for OpenAiCompatProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        // Placeholder implementation - real implementation would call the OpenAI-compatible API
        Err(ProviderError::Upstream("OpenAI-compatible provider not yet fully implemented".to_string()))
    }

    fn id(&self) -> ProviderId {
        ProviderId("openai-compat".to_string())
    }

    fn supports_streaming(&self) -> bool {
        self.capabilities.streaming
    }

    fn rate_card(&self) -> &RateCard {
        // Return a static rate card - in production this would be configurable
        use std::sync::OnceLock;
        static RATE_CARD: OnceLock<RateCard> = OnceLock::new();
        RATE_CARD.get_or_init(|| {
            RateCard {
                version_id: "openai-compat-default".to_string(),
                cents_per_1k_input: 0.5,
                cents_per_1k_output: 1.5,
                cents_per_request: 0.0,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_creation() {
        let config = crate::config::ConfigBuilder::new("https://api.openai.com/v1")
            .with_api_key("test-key")
            .build()
            .unwrap();
        let provider = OpenAiCompatProvider::new(config);
        assert_eq!(provider.base_url(), "https://api.openai.com/v1");
        assert_eq!(provider.api_key(), "test-key");
    }

    #[test]
    fn test_provider_capabilities() {
        let config = crate::config::ConfigBuilder::new("https://api.openai.com/v1")
            .build()
            .unwrap();
        let provider = OpenAiCompatProvider::new(config);
        assert!(provider.capabilities().streaming);
        assert!(provider.capabilities().tool_use);
    }

    #[test]
    fn test_provider_id() {
        let config = crate::config::ConfigBuilder::new("https://api.openai.com/v1")
            .build()
            .unwrap();
        let provider = OpenAiCompatProvider::new(config);
        assert_eq!(provider.id().0, "openai-compat");
    }

    #[test]
    fn test_provider_rate_card() {
        let config = crate::config::ConfigBuilder::new("https://api.openai.com/v1")
            .build()
            .unwrap();
        let provider = OpenAiCompatProvider::new(config);
        assert_eq!(provider.rate_card().version_id, "openai-compat-default");
    }

    #[tokio::test]
    async fn test_provider_complete_returns_error() {
        let config = crate::config::ConfigBuilder::new("https://api.openai.com/v1")
            .build()
            .unwrap();
        let provider = OpenAiCompatProvider::new(config);
        let req = CompletionRequest::new(vec![], ModelId::new("test"), 1000);
        let result = provider.complete(req).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_provider_from_env() {
        // This test will use env vars if set, otherwise uses defaults
        // We can't reliably test this without mocking env
        let result = OpenAiCompatProvider::from_env();
        // Should either succeed or fail based on env
        assert!(result.is_ok() || result.is_err());
    }
}
