use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiCompatConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_secs: u64,
}

impl OpenAiCompatConfig {
    pub fn validate_url(&self) -> crate::error::Result<()> {
        let url = Url::parse(&self.base_url).map_err(|e| {
            crate::error::OpenAiCompatError::InvalidUrl(format!("{e}"))
        })?;

        // Require HTTPS for production URLs
        if url.scheme() != "https" {
            // Allow http only for localhost/loopback in development
            let host = url.host_str().unwrap_or("");
            let is_loopback = host == "localhost" 
                || host == "127.0.0.1" 
                || host.starts_with("[::1]")
                || host.starts_with("10.")
                || host.starts_with("192.168.")
                || (host.starts_with("172.") && {
                    if let Some(second_octet) = host.split('.').nth(1) {
                        if let Ok(n) = second_octet.parse::<u8>() {
                            n >= 16 && n <= 31
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                });

            if !is_loopback {
                return Err(crate::error::OpenAiCompatError::HttpsRequired(
                    self.base_url.clone(),
                ));
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ConfigBuilder {
    base_url: String,
    api_key: String,
    model: String,
    timeout_secs: u64,
}

impl ConfigBuilder {
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.to_string(),
            api_key: String::new(),
            model: "gpt-4".to_string(),
            timeout_secs: 30,
        }
    }

    pub fn with_api_key(mut self, key: &str) -> Self {
        self.api_key = key.to_string();
        self
    }

    pub fn with_model(mut self, model: &str) -> Self {
        self.model = model.to_string();
        self
    }

    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    pub fn build(self) -> crate::error::Result<OpenAiCompatConfig> {
        let config = OpenAiCompatConfig {
            base_url: self.base_url,
            api_key: self.api_key,
            model: self.model,
            timeout_secs: self.timeout_secs,
        };
        config.validate_url()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_https_url_valid() {
        let config = OpenAiCompatConfig {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: "test".to_string(),
            model: "gpt-4".to_string(),
            timeout_secs: 30,
        };
        assert!(config.validate_url().is_ok());
    }

    #[test]
    fn test_http_localhost_allowed() {
        let config = OpenAiCompatConfig {
            base_url: "http://localhost:8080".to_string(),
            api_key: "test".to_string(),
            model: "gpt-4".to_string(),
            timeout_secs: 30,
        };
        assert!(config.validate_url().is_ok());
    }

    #[test]
    fn test_http_127_0_0_1_allowed() {
        let config = OpenAiCompatConfig {
            base_url: "http://127.0.0.1:8080".to_string(),
            api_key: "test".to_string(),
            model: "gpt-4".to_string(),
            timeout_secs: 30,
        };
        assert!(config.validate_url().is_ok());
    }

    #[test]
    fn test_http_external_rejected() {
        let config = OpenAiCompatConfig {
            base_url: "http://example.com".to_string(),
            api_key: "test".to_string(),
            model: "gpt-4".to_string(),
            timeout_secs: 30,
        };
        assert!(config.validate_url().is_err());
    }

    #[test]
    fn test_invalid_url_rejected() {
        let config = OpenAiCompatConfig {
            base_url: "not-a-url".to_string(),
            api_key: "test".to_string(),
            model: "gpt-4".to_string(),
            timeout_secs: 30,
        };
        assert!(config.validate_url().is_err());
    }

    #[test]
    fn test_builder_valid() {
        let config = ConfigBuilder::new("https://api.openai.com/v1")
            .with_api_key("test-key")
            .build()
            .unwrap();
        assert_eq!(config.api_key, "test-key");
    }

    #[test]
    fn test_builder_http_external_rejected() {
        let result = ConfigBuilder::new("http://example.com").build();
        assert!(result.is_err());
    }
}
