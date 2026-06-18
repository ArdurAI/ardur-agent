use crate::error::WebhookError;
use crate::event::WebhookEvent;
use reqwest::{Client, StatusCode};
use secrecy::SecretString;
use std::time::Duration;
use tracing::{debug, warn};

/// Configuration for the outbound webhook client.
#[derive(Debug, Clone)]
pub struct OutboundWebhookConfig {
    /// Target URL.
    pub url: String,
    /// HMAC-SHA256 signing secret (hex or raw).
    pub secret: SecretString,
    /// Request timeout.
    pub timeout: Duration,
    /// Max retry attempts on transient failures.
    pub max_retries: u32,
    /// Signature header name.
    pub signature_header: String,
}

impl OutboundWebhookConfig {
    /// Create a config with sensible defaults (3 retries, 30s timeout).
    pub fn new(url: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            secret: SecretString::new(secret.into().into()),
            timeout: Duration::from_secs(30),
            max_retries: 3,
            signature_header: "x-webhook-signature".to_string(),
        }
    }

    /// Set a custom timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set max retries.
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Set a custom signature header name.
    pub fn with_signature_header(mut self, header: impl Into<String>) -> Self {
        self.signature_header = header.into();
        self
    }
}

/// Outbound webhook client with retry and signature generation.
#[derive(Debug, Clone)]
pub struct OutboundWebhookClient {
    config: OutboundWebhookConfig,
    client: Client,
}

impl OutboundWebhookClient {
    /// Create a new client from the given config.
    pub fn new(config: OutboundWebhookConfig) -> Result<Self, WebhookError> {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| WebhookError::Internal(format!("reqwest client build failed: {e}")))?;
        Ok(Self { config, client })
    }

    /// Send a webhook event.
    ///
    /// Serializes the payload to JSON, signs the body with HMAC-SHA256,
    /// attaches the signature header, and retries on transient (5xx / timeout) failures.
    pub async fn send(&self, event: &WebhookEvent) -> Result<(), WebhookError> {
        let body = serde_json::to_vec(&event.payload)
            .map_err(|e| WebhookError::PayloadParseFailed(e.to_string()))?;
        let signature = crate::inbound::sign_body(&body, &self.config.secret)?;

        let mut last_err = None;
        for attempt in 0..=self.config.max_retries {
            let request = self
                .client
                .post(&self.config.url)
                .header(&self.config.signature_header, &signature)
                .header("content-type", "application/json")
                .body(body.clone());

            debug!(
                "outbound webhook attempt {}/{} to {}",
                attempt + 1,
                self.config.max_retries + 1,
                self.config.url
            );

            match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return Ok(());
                    }
                    if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
                        warn!("transient error {} from {}", status, self.config.url);
                        last_err = Some(WebhookError::OutboundRequestFailed(format!(
                            "HTTP {}",
                            status
                        )));
                    } else {
                        return Err(WebhookError::OutboundRequestFailed(format!(
                            "HTTP {}",
                            status
                        )));
                    }
                }
                Err(e) => {
                    warn!("request error to {}: {}", self.config.url, e);
                    last_err = Some(WebhookError::OutboundRequestFailed(e.to_string()));
                }
            }

            if attempt < self.config.max_retries {
                let backoff = Duration::from_millis(100 * 2_u64.pow(attempt));
                tokio::time::sleep(backoff).await;
            }
        }

        Err(last_err.unwrap_or_else(|| {
            WebhookError::OutboundRequestFailed("exhausted retries".to_string())
        }))
    }

    /// Build a signed request without sending it (useful for testing or custom transport).
    pub fn build_request(
        &self,
        event: &WebhookEvent,
    ) -> Result<(Vec<u8>, Vec<(String, String)>), WebhookError> {
        let body = serde_json::to_vec(&event.payload)
            .map_err(|e| WebhookError::PayloadParseFailed(e.to_string()))?;
        let signature = crate::inbound::sign_body(&body, &self.config.secret)?;
        let headers = vec![
            (self.config.signature_header.clone(), signature),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        Ok((body, headers))
    }
}
