//! [`AnthropicProvider`] — the Anthropic backend.
//!
//! Phase 1 ships a real [Messages API] client built on `reqwest`. A
//! [`AnthropicProvider::stub`] constructor preserves the previous deterministic
//! placeholder so tests can exercise the contract surface without a network
//! call; [`AnthropicProvider::new`] and [`AnthropicProvider::from_env`] build the
//! live HTTP path.
//!
//! [Messages API]: https://docs.anthropic.com/en/api/messages
//
// TODO §3.0 Phase 2: token streaming (server-sent events), `tool_use` content
// blocks parsed into [`ToolCall`]s, and multi-turn cost projection at admission.

use ardur_runtime::{CostTuple, ProviderId, Role};
use async_trait::async_trait;
use serde::Deserialize;

use crate::error::ProviderError;
use crate::provider::Provider;
use crate::rate_card::RateCard;
use crate::types::{CompletionRequest, CompletionResponse, FinishReason, ModelId, Usage};

/// The Anthropic Messages API endpoint.
const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
/// The API version header value pinned for the Messages API contract used here.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// How a provider instance services [`complete`](AnthropicProvider::complete).
enum Backend {
    /// Deterministic placeholder: no network, returns `"[anthropic stub]"`.
    Stub,
    /// Live HTTP path against the Anthropic Messages API.
    Live(reqwest::Client),
}

/// The Anthropic provider.
///
/// Construct it three ways: [`AnthropicProvider::new`] / [`from_env`] build the
/// live HTTP client; [`AnthropicProvider::stub`] returns the deterministic
/// placeholder used by unit tests.
///
/// [`from_env`]: AnthropicProvider::from_env
pub struct AnthropicProvider {
    api_key: String,
    model_id: ModelId,
    rate_card: RateCard,
    backend: Backend,
}

impl AnthropicProvider {
    /// Construct a *live* provider bound to `api_key` and a default `model_id`,
    /// priced by [`RateCard::anthropic_2026_q2_v1`].
    pub fn new(api_key: impl Into<String>, model_id: ModelId) -> Self {
        Self {
            api_key: api_key.into(),
            model_id,
            rate_card: RateCard::anthropic_2026_q2_v1(),
            backend: Backend::Live(reqwest::Client::new()),
        }
    }

    /// Construct a *live* provider, reading the API key from `ANTHROPIC_API_KEY`.
    ///
    /// Returns [`ProviderError::Unauthorized`] if the variable is unset or empty.
    pub fn from_env(model_id: ModelId) -> Result<Self, ProviderError> {
        match std::env::var("ANTHROPIC_API_KEY") {
            Ok(key) if !key.is_empty() => Ok(Self::new(key, model_id)),
            _ => Err(ProviderError::Unauthorized),
        }
    }

    /// Construct a *stub* provider: [`complete`](Self::complete) short-circuits
    /// to a fixed `"[anthropic stub]"` response and never touches the network.
    pub fn stub(model_id: ModelId) -> Self {
        Self {
            api_key: String::new(),
            model_id,
            rate_card: RateCard::anthropic_2026_q2_v1(),
            backend: Backend::Stub,
        }
    }

    /// The model this provider defaults completions to.
    #[must_use]
    pub fn model_id(&self) -> &ModelId {
        &self.model_id
    }

    /// The fixed Phase-1 stub completion.
    fn stub_response() -> CompletionResponse {
        CompletionResponse {
            content: "[anthropic stub]".to_string(),
            finish_reason: FinishReason::Stop,
            usage: Usage {
                tokens_in: 0,
                tokens_out: 0,
            },
            cost: CostTuple::default(),
            raw_provider_response: None,
        }
    }

    /// Issue the live Messages-API call and map the result onto the crate's
    /// request/response/error contract.
    async fn complete_live(
        &self,
        client: &reqwest::Client,
        req: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        if self.api_key.is_empty() {
            return Err(ProviderError::Unauthorized);
        }

        let body = build_request_body(&req);

        let resp = client
            .post(MESSAGES_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::NetworkFailure(e.to_string()))?;

        let status = resp.status();
        if status.is_success() {
            let value: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| ProviderError::Upstream(format!("decoding response body: {e}")))?;
            let parsed: MessagesResponse = serde_json::from_value(value.clone())
                .map_err(|e| ProviderError::Upstream(format!("unexpected response shape: {e}")))?;
            return Ok(parsed.into_completion(&self.rate_card, value));
        }

        // Pull the back-off hint before the body consumes `resp`.
        let retry_after_ms = parse_retry_after_ms(resp.headers());
        let code = status.as_u16();
        let body = resp.text().await.unwrap_or_default();
        Err(map_http_error(code, retry_after_ms, &body, &req.model))
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        match &self.backend {
            // The stub never networks and never validates a key; the empty-key
            // → Unauthorized contract belongs to the live backend (`new("")`).
            Backend::Stub => Ok(Self::stub_response()),
            Backend::Live(client) => self.complete_live(client, req).await,
        }
    }

    fn id(&self) -> ProviderId {
        ProviderId("anthropic".to_string())
    }

    fn supports_streaming(&self) -> bool {
        // TODO §3.0 Phase 2: flip to `true` once the SSE streaming path lands.
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// Serialize a [`CompletionRequest`] into the Messages-API request body.
///
/// System messages flatten into the top-level `system` field (joined with blank
/// lines when there are several); user/assistant turns become the `messages`
/// array. `system` and `stop_sequences` are omitted when empty.
fn build_request_body(req: &CompletionRequest) -> serde_json::Value {
    let mut messages = Vec::with_capacity(req.messages.len());
    let mut system_parts = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::System => system_parts.push(m.content.clone()),
            Role::User => messages.push(serde_json::json!({
                "role": "user",
                "content": m.content,
            })),
            Role::Assistant => messages.push(serde_json::json!({
                "role": "assistant",
                "content": m.content,
            })),
        }
    }

    let mut body = serde_json::json!({
        "model": req.model.0,
        "max_tokens": req.max_tokens,
        "messages": messages,
        "temperature": req.temperature,
    });
    let map = body
        .as_object_mut()
        .expect("json! object literal is always a map");
    if !system_parts.is_empty() {
        map.insert(
            "system".to_string(),
            serde_json::Value::String(system_parts.join("\n\n")),
        );
    }
    if !req.stop_sequences.is_empty() {
        map.insert(
            "stop_sequences".to_string(),
            serde_json::json!(req.stop_sequences),
        );
    }
    body
}

/// Parse the `retry-after` header (whole seconds) into milliseconds, defaulting
/// to `0` when absent or unparseable.
fn parse_retry_after_ms(headers: &reqwest::header::HeaderMap) -> u64 {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|secs| secs.saturating_mul(1000))
        .unwrap_or(0)
}

/// Map a non-2xx HTTP response onto the crate's [`ProviderError`] taxonomy.
fn map_http_error(code: u16, retry_after_ms: u64, body: &str, model: &ModelId) -> ProviderError {
    match code {
        401 => ProviderError::Unauthorized,
        429 => ProviderError::RateLimited { retry_after_ms },
        400 => {
            if error_type(body).as_deref() == Some("invalid_request_error") {
                ProviderError::InvalidRequest(error_message(body))
            } else {
                ProviderError::Upstream(format!("HTTP 400: {body}"))
            }
        }
        404 => ProviderError::ModelNotAvailable(model.clone()),
        500..=599 => ProviderError::Upstream(format!("HTTP {code}: {body}")),
        _ => ProviderError::Upstream(format!("HTTP {code}: {body}")),
    }
}

/// Extract `error.type` from an Anthropic error body, if present.
fn error_type(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"]["type"].as_str().map(str::to_string))
}

/// Extract `error.message` from an Anthropic error body, falling back to the raw
/// body text.
fn error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.to_string())
}

/// The subset of the Anthropic Messages response this Phase-1 path reads.
#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    stop_reason: Option<String>,
    #[serde(default)]
    stop_sequence: Option<String>,
    usage: ApiUsage,
}

/// One block of the response `content` array. Phase 1 only consumes `text`
/// blocks; `tool_use` blocks are surfaced via the finish reason but not parsed.
#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    text: Option<String>,
}

/// The `usage` object the provider bills against.
#[derive(Deserialize)]
struct ApiUsage {
    input_tokens: u32,
    output_tokens: u32,
}

impl MessagesResponse {
    /// Fold the parsed response into a [`CompletionResponse`], pricing `usage`
    /// through `rate_card` and retaining `raw` for audit.
    fn into_completion(self, rate_card: &RateCard, raw: serde_json::Value) -> CompletionResponse {
        let content = self
            .content
            .iter()
            .filter(|b| b.block_type == "text")
            .filter_map(|b| b.text.as_deref())
            .collect::<Vec<_>>()
            .join("");

        let usage = Usage {
            tokens_in: self.usage.input_tokens,
            tokens_out: self.usage.output_tokens,
        };

        let finish_reason = match self.stop_reason.as_deref() {
            Some("end_turn") | None => FinishReason::Stop,
            Some("max_tokens") => FinishReason::MaxTokens,
            Some("stop_sequence") => {
                FinishReason::StopSequence(self.stop_sequence.unwrap_or_default())
            }
            // TODO §3.0 Phase 2: parse `tool_use` content blocks into ToolCalls.
            Some("tool_use") => FinishReason::ToolUse(Vec::new()),
            Some(other) => FinishReason::Error(format!("unknown stop_reason: {other}")),
        };

        CompletionResponse {
            content,
            finish_reason,
            cost: rate_card.price(usage),
            usage,
            raw_provider_response: Some(raw),
        }
    }
}
