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
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
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
    /// Optional timestamp header name for replay protection (e.g.
    /// `x-webhook-timestamp`). When set, the handler requires the header on
    /// every request, verifies the timestamp is within `replay_window_secs`
    /// of the current time, includes the timestamp in the HMAC payload
    /// (signed as `"{timestamp}.{body}"`), and rejects an already-seen
    /// signature within that window. When `None`, replay protection is disabled
    /// for backwards compatibility — only HMAC over the body is verified, and
    /// exact captured request replays cannot be distinguished.
    pub timestamp_header: Option<String>,
    /// Maximum age of a webhook request in seconds (default: 300 = 5 minutes).
    /// Only enforced when `timestamp_header` is `Some`.
    pub replay_window_secs: u64,
}

impl WebhookConfig {
    /// Create a new config with the given secret and source.
    pub fn new(secret: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            secret: SecretString::new(secret.into().into()),
            signature_header: "x-webhook-signature".to_string(),
            source: source.into(),
            timestamp_header: None,
            replay_window_secs: 300,
        }
    }

    /// Set a custom signature header name.
    pub fn with_signature_header(mut self, header: impl Into<String>) -> Self {
        self.signature_header = header.into();
        self
    }

    /// Enable replay protection using the given timestamp header name.
    ///
    /// The signature is then computed over `"{timestamp}.{body}"`, the
    /// timestamp must be within `replay_window_secs` of the current time, and
    /// each signature is accepted at most once during that window.
    pub fn with_replay_protection(mut self, timestamp_header: impl Into<String>) -> Self {
        self.timestamp_header = Some(timestamp_header.into());
        self
    }

    /// Set the replay window in seconds (default: 300).
    pub fn with_replay_window_secs(mut self, secs: u64) -> Self {
        self.replay_window_secs = secs;
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

/// Verify an HMAC-SHA256 signature over `"{timestamp}.{body}"`.
///
/// This is the replay-protected variant: the timestamp is part of the signed
/// payload, so an attacker cannot replay a captured signature with a different
/// timestamp.
pub fn verify_signature_with_timestamp(
    body: &[u8],
    timestamp: &str,
    secret: &SecretString,
    signature_hex: &str,
) -> Result<(), WebhookError> {
    let mut mac = HmacSha256::new_from_slice(secret.expose_secret().as_bytes())
        .map_err(|e| WebhookError::Internal(format!("HMAC init failed: {e}")))?;
    mac.update(timestamp.as_bytes());
    mac.update(b".");
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
pub fn sign_body(body: &[u8], secret: &SecretString) -> Result<String, WebhookError> {
    let mut mac = HmacSha256::new_from_slice(secret.expose_secret().as_bytes())
        .map_err(|e| WebhookError::Internal(format!("HMAC init failed: {e}")))?;
    mac.update(body);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

/// Sign a webhook body with a timestamp for HMAC verification.
///
/// This function is currently unused but kept for future webhook
/// inbound verification implementations.
#[allow(dead_code)]
pub fn sign_body_with_timestamp(
    body: &[u8],
    timestamp: &str,
    secret: &SecretString,
) -> Result<String, WebhookError> {
    let mut mac = HmacSha256::new_from_slice(secret.expose_secret().as_bytes())
        .map_err(|e| WebhookError::Internal(format!("HMAC init failed: {e}")))?;
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    Ok(hex::encode(mac.finalize().into_bytes()))
}

const DEFAULT_REPLAY_CACHE_MAX_ENTRIES: usize = 4096;

#[derive(Debug)]
struct ReplayCache {
    default_ttl: Duration,
    max_entries: usize,
    entries: HashMap<String, Instant>,
    insertion_order: VecDeque<(String, Instant)>,
}

impl ReplayCache {
    fn new(default_ttl: Duration, max_entries: usize) -> Self {
        Self {
            default_ttl,
            max_entries: max_entries.max(1),
            entries: HashMap::new(),
            insertion_order: VecDeque::new(),
        }
    }

    fn insert_new(&mut self, key: String, now: Instant) -> bool {
        self.insert_new_with_ttl(key, now, self.default_ttl)
    }

    fn insert_new_with_ttl(&mut self, key: String, now: Instant, ttl: Duration) -> bool {
        self.prune_expired(now);
        if self.entries.contains_key(&key) {
            return false;
        }

        let expires_at = now.checked_add(ttl).unwrap_or(now);
        self.entries.insert(key.clone(), expires_at);
        self.insertion_order.push_back((key, expires_at));
        self.prune_overflow();
        true
    }

    fn prune_expired(&mut self, now: Instant) {
        while let Some((key, expires_at)) = self.insertion_order.front() {
            if *expires_at > now {
                break;
            }
            let key = key.clone();
            let expires_at = *expires_at;
            self.insertion_order.pop_front();
            if self
                .entries
                .get(&key)
                .is_some_and(|current| *current == expires_at)
            {
                self.entries.remove(&key);
            }
        }
    }

    fn prune_overflow(&mut self) {
        while self.entries.len() > self.max_entries {
            if let Some((key, expires_at)) = self.insertion_order.pop_front() {
                if self
                    .entries
                    .get(&key)
                    .is_some_and(|current| *current == expires_at)
                {
                    self.entries.remove(&key);
                }
            } else {
                break;
            }
        }
    }
}

fn replay_cache() -> &'static Mutex<ReplayCache> {
    static CACHE: OnceLock<Mutex<ReplayCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(ReplayCache::new(
            Duration::from_secs(300),
            DEFAULT_REPLAY_CACHE_MAX_ENTRIES,
        ))
    })
}

fn record_replay_signature(signature: &str, window: Duration) -> Result<bool, WebhookError> {
    let mut cache = replay_cache()
        .lock()
        .map_err(|_| WebhookError::Internal("replay cache lock poisoned".to_string()))?;
    cache.default_ttl = window;
    Ok(cache.insert_new(signature.to_ascii_lowercase(), Instant::now()))
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
/// 2. Extracts the signature header (and timestamp when replay protection is on).
/// 3. Verifies the HMAC-SHA256 signature (over `"{timestamp}.{body}"` when
///    replay protection is configured, over `body` otherwise).
/// 4. When replay protection is on, rejects timestamps outside the window.
/// 5. Parses the body as JSON and emits a [`WebhookEvent`].
pub async fn receive_webhook(
    State(state): State<Arc<InboundState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let signature = headers
        .get(&state.config.signature_header)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Replay protection: when a timestamp header is configured, extract and
    // verify it, and include it in the HMAC payload.
    if let Some(ts_header) = &state.config.timestamp_header {
        let timestamp = match headers.get(ts_header).and_then(|v| v.to_str().ok()) {
            Some(ts) => ts.to_string(),
            None => {
                warn!(
                    "missing timestamp header `{ts_header}` for source={}, rejecting",
                    state.config.source
                );
                return (StatusCode::UNAUTHORIZED, "Missing timestamp").into_response();
            }
        };

        // Parse as Unix epoch seconds (decimal string).
        let ts_secs: i64 = match timestamp.parse() {
            Ok(v) => v,
            Err(_) => {
                warn!(
                    "unparseable timestamp `{timestamp}` for source={}",
                    state.config.source
                );
                return (StatusCode::BAD_REQUEST, "Invalid timestamp").into_response();
            }
        };

        let now_secs = chrono::Utc::now().timestamp();
        let window = i64::try_from(state.config.replay_window_secs).unwrap_or(i64::MAX);
        // Use checked arithmetic to avoid overflow panic on extreme i64 values
        // (e.g. i64::MIN). If the subtraction overflows, the timestamp is
        // clearly invalid — reject as stale.
        let outside_window = now_secs
            .checked_sub(ts_secs)
            .map(|diff| diff.abs() > window)
            .unwrap_or(true);
        if outside_window {
            warn!(
                "timestamp outside replay window ({}s) for source={}",
                window, state.config.source
            );
            return (StatusCode::UNAUTHORIZED, "Stale request").into_response();
        }

        if let Err(e) =
            verify_signature_with_timestamp(&body, &timestamp, &state.config.secret, signature)
        {
            warn!(
                "signature verification failed for source={}: {}",
                state.config.source, e
            );
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }

        let replay_window = Duration::from_secs(state.config.replay_window_secs.max(1));
        match record_replay_signature(signature, replay_window) {
            Ok(true) => {}
            Ok(false) => {
                warn!(
                    "replayed webhook signature for source={}, rejecting",
                    state.config.source
                );
                return (StatusCode::UNAUTHORIZED, "Replay request").into_response();
            }
            Err(e) => {
                error!(
                    "replay cache error for source={}: {}",
                    state.config.source, e
                );
                return (StatusCode::INTERNAL_SERVER_ERROR, "Replay cache error").into_response();
            }
        }
    } else if let Err(e) = verify_signature(&body, &state.config.secret, signature) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderName;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingHandler {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl InboundWebhookHandler for CountingHandler {
        async fn handle(&self, _event: WebhookEvent) -> Result<(), WebhookError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn signed_headers(config: &WebhookConfig, body: &[u8], timestamp: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let signature_header: HeaderName = config.signature_header.parse().unwrap();
        headers.insert(
            signature_header,
            sign_body_with_timestamp(body, timestamp, &config.secret)
                .unwrap()
                .parse()
                .unwrap(),
        );
        let timestamp_header: HeaderName =
            config.timestamp_header.as_deref().unwrap().parse().unwrap();
        headers.insert(timestamp_header, timestamp.parse().unwrap());
        headers
    }

    #[tokio::test]
    async fn exact_replay_is_rejected_before_handler_dispatch() {
        let config = WebhookConfig::new("super-secret", "ci")
            .with_replay_protection("x-webhook-timestamp")
            .with_replay_window_secs(300);
        let handler = Arc::new(CountingHandler {
            calls: AtomicUsize::new(0),
        });
        let state = Arc::new(InboundState {
            config: config.clone(),
            handler: handler.clone(),
        });
        let body = Bytes::from_static(br#"{"event":"push"}"#);
        let timestamp = chrono::Utc::now().timestamp().to_string();
        let headers = signed_headers(&config, &body, &timestamp);

        let first = receive_webhook(State(state.clone()), headers.clone(), body.clone())
            .await
            .into_response();
        assert_eq!(first.status(), StatusCode::OK);

        let replay = receive_webhook(State(state), headers, body)
            .await
            .into_response();
        assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn replay_cache_evicts_entries_after_window() {
        let now = Instant::now();
        let mut cache = ReplayCache::new(Duration::from_secs(1), 16);

        assert!(cache.insert_new("sig-a".to_string(), now));
        assert!(!cache.insert_new("sig-a".to_string(), now + Duration::from_millis(500)));
        assert!(cache.insert_new("sig-a".to_string(), now + Duration::from_secs(2)));
    }
}
