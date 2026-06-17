use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type ProviderId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProviderStatus {
    Available,
    Busy,
    Offline,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub max_tokens: usize,
    pub supports_streaming: bool,
    pub supports_tools: bool,
    pub cost_per_1k_input: f64,
    pub cost_per_1k_output: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub api_key: String,
    pub base_url: String,
    pub timeout_seconds: u64,
    pub max_retries: u32,
    pub default_model: String,
    pub custom_headers: HashMap<String, String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: "https://api.openai.com/v1".to_string(),
            timeout_seconds: 30,
            max_retries: 3,
            default_model: "gpt-4".to_string(),
            custom_headers: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: ProviderId,
    pub name: String,
    pub provider_type: String,
    pub status: ProviderStatus,
    pub config: ProviderConfig,
    pub models: Vec<ModelInfo>,
    pub created_at: DateTime<Utc>,
    pub last_used: Option<DateTime<Utc>>,
    pub usage_count: u64,
    pub metadata: HashMap<String, String>,
}

impl Provider {
    pub fn new(name: &str, provider_type: &str) -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            name: name.to_string(),
            provider_type: provider_type.to_string(),
            status: ProviderStatus::Available,
            config: ProviderConfig::default(),
            models: Vec::new(),
            created_at: Utc::now(),
            last_used: None,
            usage_count: 0,
            metadata: HashMap::new(),
        }
    }

    pub fn with_config(mut self, config: ProviderConfig) -> Self {
        self.config = config;
        self
    }

    pub fn add_model(mut self, model: ModelInfo) -> Self {
        self.models.push(model);
        self
    }

    pub fn mark_used(&mut self) {
        self.last_used = Some(Utc::now());
        self.usage_count += 1;
    }

    pub fn set_status(&mut self, status: ProviderStatus) {
        self.status = status;
    }

    pub fn find_model(&self, model_id: &str) -> Option<&ModelInfo> {
        self.models.iter().find(|m| m.id == model_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_creation() {
        let provider = Provider::new("OpenAI", "openai");
        assert_eq!(provider.name, "OpenAI");
        assert_eq!(provider.provider_type, "openai");
        assert_eq!(provider.status, ProviderStatus::Available);
    }

    #[test]
    fn test_provider_with_config() {
        let mut config = ProviderConfig::default();
        config.api_key = "sk-test".to_string();
        let provider = Provider::new("OpenAI", "openai").with_config(config);
        assert_eq!(provider.config.api_key, "sk-test");
    }

    #[test]
    fn test_provider_add_model() {
        let model = ModelInfo {
            id: "gpt-4".to_string(),
            name: "GPT-4".to_string(),
            provider: "openai".to_string(),
            max_tokens: 8192,
            supports_streaming: true,
            supports_tools: true,
            cost_per_1k_input: 0.03,
            cost_per_1k_output: 0.06,
        };
        let provider = Provider::new("OpenAI", "openai").add_model(model);
        assert_eq!(provider.models.len(), 1);
        assert_eq!(provider.models[0].id, "gpt-4");
    }

    #[test]
    fn test_provider_mark_used() {
        let mut provider = Provider::new("OpenAI", "openai");
        provider.mark_used();
        assert_eq!(provider.usage_count, 1);
        assert!(provider.last_used.is_some());
    }

    #[test]
    fn test_provider_find_model() {
        let model = ModelInfo {
            id: "gpt-4".to_string(),
            name: "GPT-4".to_string(),
            provider: "openai".to_string(),
            max_tokens: 8192,
            supports_streaming: true,
            supports_tools: true,
            cost_per_1k_input: 0.03,
            cost_per_1k_output: 0.06,
        };
        let provider = Provider::new("OpenAI", "openai").add_model(model);
        assert!(provider.find_model("gpt-4").is_some());
        assert!(provider.find_model("gpt-3").is_none());
    }
}
