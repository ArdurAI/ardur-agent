use crate::error::WebhookError;
use crate::event::{EventType, WebhookEvent};
use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use hmac::{Hmac, KeyInit, Mac};
use secrecy::{ExposeSecret, SecretString};
use sha2::Sha256;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tracing::{error, warn};

type HmacSha256 = Hmac<Sha256>;

/// Configuration for an inbound webhook endpoint.
#[derive(Debug, Clone)]
pub struct WebhookConfig {
    /// The HMAC-SHA256 signing secret (hex-encoded or raw bytes).
    pub secret: SecretString,
    /// Optional expected signature header name (default: `x-webhook-signature`).
    pub signature_header: String,
    /// Source identifier emitted on the [`WebhookEvent`].
    pub source: String,
}

impl WebhookConfig {
    /// Create a new config with the given secret and source.
    pub fn new(secret: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            secret: SecretString::new(secret.into().into()),
            signature_header: "x-webhook-signature".to_string(),
            source: source.into(),
        }
    }

    /// Set a custom signature header name.
    pub fn with_signature_header(mut self, header: impl Into<String>) -> Self {
        self.signature_header = header.into();
        self
    }
}

/// Trait for processing inbound webhooks.
#[async_trait]
pub trait InboundWebhookHandler: Send + Sync {
    /// Process the verified webhook event.
    async fn handle(&self, event: WebhookEvent) -> Result<(), WebhookError>;
}

/// Verify an HMAC-SHA256 signature over a raw body.
///
/// `signature_hex` is the expected hex-encoded HMAC digest.
/// Returns `Ok(())` if the signature matches, otherwise [`WebhookError::SignatureVerificationFailed`].
pub fn verify_signature(
    body: &[u8],
    secret: &SecretString,
    signature_hex: &str,
) -> Result<(), WebhookError> {
    let mut mac = HmacSha256::new_from_slice(secret.expose_secret().as_bytes())
        .map_err(|e| WebhookError::Internal(format!("HMAC init failed: {e}")))?;
    mac.update(body);
    let expected =
        hex::decode(signature_hex).map_err(|_| WebhookError::SignatureVerificationFailed)?;
    let computed = mac.finalize().into_bytes();
    if expected.len() != computed.len() {
        return Err(WebhookError::SignatureVerificationFailed);
    }
    if expected.ct_eq(&computed).into() {
        Ok(())
    } else {
        Err(WebhookError::SignatureVerificationFailed)
    }
}

/// Generate an HMAC-SHA256 signature for a body (hex-encoded).
pub fn sign_body(body: &[u8], secret: &SecretString) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.expose_secret().as_bytes())
        .expect("HMAC init with any length key is infallible for HmacSha256");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// Axum state shared with the inbound handler.
pub struct InboundState {
    /// The webhook configuration (secret, header name, source).
    pub config: WebhookConfig,
    /// The handler to emit the parsed event to.
    pub handler: Arc<dyn InboundWebhookHandler>,
}

/// Axum handler for receiving inbound webhooks.
///
/// 1. Reads the raw body.
/// 2. Extracts the signature header.
/// 3. Verifies the HMAC-SHA256 signature.
/// 4. Parses the body as JSON and emits a [`WebhookEvent`].
pub async fn receive_webhook(
    State(state): State<Arc<InboundState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let signature = headers
        .get(&state.config.signature_header)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if let Err(e) = verify_signature(&body, &state.config.secret, signature) {
        warn!(
            "signature verification failed for source={}: {}",
            state.config.source, e
        );
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    let payload = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(v) => v,
        Err(_) => serde_json::Value::String(String::from_utf8_lossy(&body).to_string()),
    };

    let event = WebhookEvent::new(
        EventType::Custom("webhook".to_string()),
        state.config.source.clone(),
        payload,
    );

    if let Err(e) = state.handler.handle(event).await {
        error!("handler error for source={}: {}", state.config.source, e);
        return (StatusCode::INTERNAL_SERVER_ERROR, "Handler error").into_response();
    }

    (StatusCode::OK, "OK").into_response()
}
