use crate::error::WebhookError;
use crate::event::WebhookEvent;
use ardur_resilience::circuit_breaker::{CircuitBreaker, CircuitBreakerConfig, CircuitError};
use ardur_resilience::retry::{RetryPolicy, retry_with_backoff};
use reqwest::{Client, StatusCode};
use secrecy::SecretString;
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;

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
    /// The circuit breaker that trips after repeated failures and fails fast
    /// (without hitting the network) until it cools down.
    pub circuit_breaker: CircuitBreakerConfig,
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
            circuit_breaker: CircuitBreakerConfig::default(),
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

    /// Override the circuit breaker that trips after repeated failures.
    pub fn with_circuit_breaker(mut self, circuit_breaker: CircuitBreakerConfig) -> Self {
        self.circuit_breaker = circuit_breaker;
        self
    }

    /// This config's `max_retries` translated into a [`RetryPolicy`]:
    /// exponential backoff + full jitter, starting at 100ms and doubling,
    /// capped at 30s — the same shape the previous bare retry loop used,
    /// now with jitter so retries from many callers don't synchronize after
    /// a shared outage.
    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy {
            max_attempts: self.max_retries + 1,
            initial_backoff_ms: 100,
            max_backoff_ms: 30_000,
            backoff_multiplier: 2,
        }
    }
}

/// One failed delivery attempt, classifying whether it is worth retrying
/// (a transient network error, `429`, or `5xx`) or permanent (any other
/// non-2xx status — resending would just repeat the same rejection).
struct AttemptError {
    error: WebhookError,
    retryable: bool,
}

/// Outbound webhook client with retry, a circuit breaker, and signature
/// generation.
#[derive(Debug, Clone)]
pub struct OutboundWebhookClient {
    config: OutboundWebhookConfig,
    client: Client,
    /// Shared across clones (`Arc`) — cloning the client shares delivery
    /// history rather than resetting the failure count.
    breaker: Arc<CircuitBreaker>,
}

impl OutboundWebhookClient {
    /// Create a new client from the given config.
    pub fn new(config: OutboundWebhookConfig) -> Result<Self, WebhookError> {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|e| WebhookError::Internal(format!("reqwest client build failed: {e}")))?;
        let breaker = Arc::new(CircuitBreaker::new(config.circuit_breaker.clone()));
        Ok(Self {
            config,
            client,
            breaker,
        })
    }

    /// Send a webhook event.
    ///
    /// Serializes the payload to JSON, signs the body with HMAC-SHA256,
    /// attaches the signature header, and retries transient (5xx / `429` /
    /// network) failures with backoff + jitter through the shared circuit
    /// breaker — a run of failures fails fast on the next call instead of
    /// piling up further timeouts.
    pub async fn send(&self, event: &WebhookEvent) -> Result<(), WebhookError> {
        let body = serde_json::to_vec(&event.payload)
            .map_err(|e| WebhookError::PayloadParseFailed(e.to_string()))?;
        let signature = crate::inbound::sign_body(&body, &self.config.secret)?;
        let retry_policy = self.config.retry_policy();

        self.breaker
            .call(|| {
                retry_with_backoff(
                    &retry_policy,
                    |a: &AttemptError| a.retryable,
                    || self.send_once(&body, &signature),
                )
            })
            .await
            .map_err(|e| match e {
                CircuitError::Open => WebhookError::OutboundRequestFailed(format!(
                    "circuit breaker open: too many recent delivery failures to {}",
                    self.config.url
                )),
                CircuitError::Inner(attempt) => attempt.error,
            })
    }

    /// One delivery attempt.
    async fn send_once(&self, body: &[u8], signature: &str) -> Result<(), AttemptError> {
        debug!("outbound webhook attempt to {}", self.config.url);
        let request = self
            .client
            .post(&self.config.url)
            .header(&self.config.signature_header, signature)
            .header("content-type", "application/json")
            .body(body.to_vec());

        match request.send().await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    return Ok(());
                }
                let retryable = status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS;
                Err(AttemptError {
                    error: WebhookError::OutboundRequestFailed(format!("HTTP {status}")),
                    retryable,
                })
            }
            Err(e) => Err(AttemptError {
                error: WebhookError::OutboundRequestFailed(e.to_string()),
                retryable: true,
            }),
        }
    }

    /// Build a signed request without sending it (useful for testing or custom transport).
    #[allow(clippy::type_complexity)]
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
