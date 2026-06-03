//! ardur-provider-openrouter — the [OpenRouter] multi-model gateway backend
//! (§3.2).
//!
//! OpenRouter exposes a single OpenAI-compatible `POST /chat/completions`
//! endpoint that routes to many upstream model providers (Anthropic, OpenAI,
//! Google, Meta, Mistral, …) behind one API key. This crate implements the §3.0
//! [`Provider`] trait against that endpoint, so the runtime can dispatch a turn
//! to *any* OpenRouter-listed model by passing its slug through the request's
//! [`ModelId`] — e.g. `anthropic/claude-3.5-sonnet`, `openai/gpt-4o`,
//! `google/gemini-flash-1.5`, `meta-llama/llama-3.1-8b-instruct:free`,
//! `mistralai/mistral-7b-instruct`. The model string is opaque to this layer and
//! passes through unchanged; OpenRouter validates it.
//!
//! # Phase 1 (this crate)
//!
//! - [`OpenRouterProvider`] — the backend. [`OpenRouterProvider::new`] /
//!   [`OpenRouterProvider::from_env`] build the live HTTP path.
//! - [`OpenRouterConfig`] — the connection config (api key, base URL, the
//!   recommended `HTTP-Referer` / `X-Title` attribution headers, request
//!   timeout), built with a small builder.
//! - The [`Provider::complete`] impl translates the runtime's
//!   [`CompletionRequest`] into the OpenAI chat-completions request body and the
//!   OpenAI response back into a [`CompletionResponse`], pricing the call from
//!   the `usage.cost` OpenRouter reports (USD → whole US cents; `0` when
//!   absent — Phase 1 does not reconstruct cost from a rate card).
//!
//! # Not in Phase 1
//!
//! - **Streaming** — every request sends `stream: false`;
//!   [`Provider::supports_streaming`] is `false`. (Phase 2.)
//! - **Tool-call parsing** — a `tool_calls` finish reason surfaces as
//!   [`FinishReason::ToolUse`] with an empty call list; the blocks are not
//!   decoded yet. (Phase 2.)
//!
//! [OpenRouter]: https://openrouter.ai
//! [`ModelId`]: ardur_provider_runtime::ModelId
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::time::Duration;

use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, ModelId, Provider, ProviderError,
    RateCard, Usage,
};
use ardur_runtime::{CostTuple, ProviderId, Role};
use async_trait::async_trait;
use serde::Deserialize;

/// The registry key this backend answers to.
const PROVIDER_ID: &str = "openrouter";
/// OpenRouter's OpenAI-compatible API base URL.
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
/// The default `HTTP-Referer` attribution header OpenRouter recommends.
pub const DEFAULT_REFERER: &str = "https://github.com/ArdurAI/ardur-agent";
/// The default `X-Title` attribution header OpenRouter recommends.
pub const DEFAULT_TITLE: &str = "Ardur Agent";
/// The default per-request timeout.
const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// The environment variable the API key is read from by
/// [`OpenRouterConfig::from_env`] / [`OpenRouterProvider::from_env`].
pub const API_KEY_ENV: &str = "OPENROUTER_API_KEY";

/// How an [`OpenRouterProvider`] connects to the gateway.
///
/// Build it from an API key with [`OpenRouterConfig::new`] (or
/// [`OpenRouterConfig::from_env`]) and tune the optional fields with the
/// builder methods; every field but the key has a sensible default.
#[derive(Clone, Debug)]
pub struct OpenRouterConfig {
    api_key: String,
    base_url: String,
    referer: String,
    title: String,
    request_timeout: Duration,
}

impl OpenRouterConfig {
    /// A config bound to `api_key`, with the default base URL, attribution
    /// headers, and request timeout.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            referer: DEFAULT_REFERER.to_string(),
            title: DEFAULT_TITLE.to_string(),
            request_timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }

    /// Read the API key from [`API_KEY_ENV`] (`OPENROUTER_API_KEY`).
    ///
    /// Returns [`ProviderError::Unauthorized`] when the variable is unset or
    /// empty — the "missing API key" config error.
    pub fn from_env() -> Result<Self, ProviderError> {
        match std::env::var(API_KEY_ENV) {
            Ok(key) if !key.is_empty() => Ok(Self::new(key)),
            _ => Err(ProviderError::Unauthorized),
        }
    }

    /// Override the API base URL (e.g. to point at a mock server in tests).
    /// A trailing slash is trimmed so the `/chat/completions` join is stable.
    #[must_use]
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Override the `HTTP-Referer` attribution header.
    #[must_use]
    pub fn referer(mut self, referer: impl Into<String>) -> Self {
        self.referer = referer.into();
        self
    }

    /// Override the `X-Title` attribution header.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Override the per-request timeout.
    #[must_use]
    pub fn request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    /// The `chat/completions` endpoint URL for this config's base.
    fn completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

/// The OpenRouter provider.
///
/// Construct it with [`OpenRouterProvider::new`] (from an [`OpenRouterConfig`]
/// and a default model) or [`OpenRouterProvider::from_env`] (reading the API key
/// from `OPENROUTER_API_KEY`). The model on each [`CompletionRequest`] selects
/// which upstream model OpenRouter routes to; `model_id` is only the default the
/// runtime stamps onto a request.
pub struct OpenRouterProvider {
    config: OpenRouterConfig,
    model_id: ModelId,
    rate_card: RateCard,
    client: reqwest::Client,
}

impl OpenRouterProvider {
    /// Build a live provider from `config` with a default `model_id`.
    #[must_use]
    pub fn new(config: OpenRouterConfig, model_id: ModelId) -> Self {
        Self {
            config,
            model_id,
            rate_card: openrouter_passthrough_rate_card(),
            client: reqwest::Client::new(),
        }
    }

    /// Build a live provider with a default `model_id`, reading the API key from
    /// `OPENROUTER_API_KEY`.
    ///
    /// Returns [`ProviderError::Unauthorized`] if the variable is unset or empty.
    pub fn from_env(model_id: ModelId) -> Result<Self, ProviderError> {
        Ok(Self::new(OpenRouterConfig::from_env()?, model_id))
    }

    /// The model this provider defaults completions to.
    #[must_use]
    pub fn model_id(&self) -> &ModelId {
        &self.model_id
    }
}

#[async_trait]
impl Provider for OpenRouterProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        if self.config.api_key.is_empty() {
            return Err(ProviderError::Unauthorized);
        }

        let body = build_request_body(&req);

        let resp = self
            .client
            .post(self.config.completions_url())
            .bearer_auth(&self.config.api_key)
            .header("HTTP-Referer", &self.config.referer)
            .header("X-Title", &self.config.title)
            .timeout(self.config.request_timeout)
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
            let parsed: ChatCompletion = serde_json::from_value(value.clone())
                .map_err(|e| ProviderError::Upstream(format!("unexpected response shape: {e}")))?;
            return Ok(parsed.into_completion(value));
        }

        // Pull the back-off hint before the body consumes `resp`.
        let retry_after_ms = parse_retry_after_ms(resp.headers());
        let code = status.as_u16();
        let body = resp.text().await.unwrap_or_default();
        Err(map_http_error(code, retry_after_ms, &body, &req.model))
    }

    fn id(&self) -> ProviderId {
        ProviderId(PROVIDER_ID.to_string())
    }

    fn supports_streaming(&self) -> bool {
        // Phase 2: flip to `true` once the SSE streaming path lands.
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// A zeroed "passthrough" rate card.
///
/// OpenRouter reports the *actual* dollar cost of each call in its `usage.cost`
/// field, which [`ChatCompletion::into_completion`] maps straight to the billed
/// cents — so this provider never prices a call from a per-1k rate card. The
/// card exists only to satisfy [`Provider::rate_card`]; its rates are `0`.
// TODO §3.2 Phase 2: track actual per-model OpenRouter pricing here as a
// fallback for the rare response that omits `usage.cost`.
fn openrouter_passthrough_rate_card() -> RateCard {
    RateCard {
        version_id: "openrouter-passthrough-v1".to_string(),
        cents_per_1k_input: 0.0,
        cents_per_1k_output: 0.0,
        cents_per_request: 0.0,
    }
}

/// Serialize a [`CompletionRequest`] into the OpenAI chat-completions body.
///
/// Unlike the Anthropic Messages API, the OpenAI shape keeps `system` turns
/// inline in the `messages` array, so every role maps one-to-one. `stop` is
/// omitted when there are no stop sequences. `stream` is pinned `false` — Phase
/// 1 is non-streaming.
fn build_request_body(req: &CompletionRequest) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = req
        .messages
        .iter()
        .map(|m| {
            serde_json::json!({
                "role": role_str(m.role),
                "content": m.content,
            })
        })
        .collect();

    let mut body = serde_json::json!({
        "model": req.model.0,
        "messages": messages,
        "max_tokens": req.max_tokens,
        "temperature": req.temperature,
        "stream": false,
    });
    let map = body
        .as_object_mut()
        .expect("json! object literal is always a map");
    if !req.stop_sequences.is_empty() {
        map.insert("stop".to_string(), serde_json::json!(req.stop_sequences));
    }
    body
}

/// The OpenAI/`messages` role wire string for a [`Role`].
fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
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

/// Map a non-2xx OpenRouter response onto the crate's [`ProviderError`]
/// taxonomy. OpenRouter's error body is `{ "error": { "message", "code", … } }`;
/// its message (and code, when present) are surfaced in the mapped error.
fn map_http_error(code: u16, retry_after_ms: u64, body: &str, model: &ModelId) -> ProviderError {
    let err = ApiErrorBody::extract(body);
    match code {
        401 | 403 => ProviderError::Unauthorized,
        429 => ProviderError::RateLimited { retry_after_ms },
        400 => ProviderError::InvalidRequest(err.describe()),
        404 => ProviderError::ModelNotAvailable(model.clone()),
        _ => ProviderError::Upstream(format!("HTTP {code}: {}", err.describe())),
    }
}

/// The `{ "error": { … } }` envelope OpenRouter returns on a failed call.
#[derive(Default)]
struct ApiErrorBody {
    message: Option<String>,
    code: Option<String>,
}

impl ApiErrorBody {
    /// Pull `error.message` and `error.code` out of an error body, tolerating a
    /// `code` that arrives as either a JSON number or a string.
    fn extract(body: &str) -> Self {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
            return Self::default();
        };
        let error = &value["error"];
        let message = error["message"].as_str().map(str::to_string);
        let code = match &error["code"] {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        };
        Self { message, code }
    }

    /// A human-readable "<message> (code: <code>)" rendering, falling back to a
    /// generic note when the body carried neither field.
    fn describe(&self) -> String {
        match (&self.message, &self.code) {
            (Some(m), Some(c)) => format!("{m} (code: {c})"),
            (Some(m), None) => m.clone(),
            (None, Some(c)) => format!("openrouter error (code: {c})"),
            (None, None) => "openrouter error with no message".to_string(),
        }
    }
}

/// The subset of the OpenAI chat-completions response this Phase-1 path reads.
#[derive(Deserialize)]
struct ChatCompletion {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

/// One entry of the response `choices` array.
#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    message: Message,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// The assistant message inside a [`Choice`]. `content` is optional because a
/// tool-call-only turn carries `null`.
#[derive(Default, Deserialize)]
struct Message {
    #[serde(default)]
    content: Option<String>,
}

/// The `usage` object OpenRouter bills against. `cost` is the dollar cost of the
/// call (OpenRouter-specific; absent on some upstreams).
#[derive(Deserialize)]
struct ApiUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    cost: Option<f64>,
}

impl ChatCompletion {
    /// Fold the parsed response into a [`CompletionResponse`], retaining `raw`
    /// for audit. Token counts come from `usage`; the billed `cents` are derived
    /// from OpenRouter's reported `usage.cost` (USD → whole cents), `0` when the
    /// field is absent.
    fn into_completion(self, raw: serde_json::Value) -> CompletionResponse {
        let first = self.choices.into_iter().next();
        let content = first
            .as_ref()
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default();
        let finish_reason = map_finish_reason(first.and_then(|c| c.finish_reason).as_deref());

        let (usage, cents) = match self.usage {
            Some(u) => {
                let usage = Usage {
                    tokens_in: u.prompt_tokens,
                    tokens_out: u.completion_tokens,
                };
                (usage, dollars_to_cents(u.cost))
            }
            None => (Usage::default(), 0),
        };

        let cost = CostTuple {
            tokens_in: u64::from(usage.tokens_in),
            tokens_out: u64::from(usage.tokens_out),
            cents,
            wall_ms: 0,
            attention_score: 0.0,
        };

        CompletionResponse {
            content,
            finish_reason,
            usage,
            cost,
            raw_provider_response: Some(raw),
        }
    }
}

/// Map the OpenAI `finish_reason` onto the crate's [`FinishReason`]. A missing
/// reason (`null`) is treated as a natural stop.
fn map_finish_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("stop") | None => FinishReason::Stop,
        Some("length") => FinishReason::MaxTokens,
        // Phase 2: decode the `tool_calls` blocks into ToolCalls.
        Some("tool_calls") => FinishReason::ToolUse(Vec::new()),
        Some("content_filter") => {
            FinishReason::Error("generation halted by content filter".to_string())
        }
        Some(other) => FinishReason::Error(format!("unknown finish_reason: {other}")),
    }
}

/// Convert an optional dollar cost into whole US cents, rounding to the nearest
/// cent. A missing cost (the field OpenRouter omits on some upstreams) is `0`.
fn dollars_to_cents(cost: Option<f64>) -> u64 {
    match cost {
        Some(dollars) if dollars.is_finite() && dollars > 0.0 => (dollars * 100.0).round() as u64,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_body_maps_roles_and_pins_stream_false() {
        use ardur_runtime::ChatMessage;
        let mut req = CompletionRequest::new(
            vec![
                ChatMessage::system("be terse"),
                ChatMessage::user("hi"),
                ChatMessage::assistant("hello"),
            ],
            ModelId::new("openai/gpt-4o"),
            128,
        );
        req.temperature = 0.5;
        req.stop_sequences = vec!["STOP".to_string()];

        let body = build_request_body(&req);
        assert_eq!(body["model"], "openai/gpt-4o");
        assert_eq!(body["stream"], false);
        assert_eq!(body["max_tokens"], 128);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][2]["role"], "assistant");
        assert_eq!(body["stop"][0], "STOP");
    }

    #[test]
    fn request_body_omits_stop_when_empty() {
        use ardur_runtime::ChatMessage;
        let req = CompletionRequest::new(
            vec![ChatMessage::user("hi")],
            ModelId::new("openai/gpt-4o"),
            16,
        );
        let body = build_request_body(&req);
        assert!(
            body.get("stop").is_none(),
            "no stop key when none requested"
        );
    }

    #[test]
    fn dollars_round_to_nearest_cent_and_default_zero() {
        assert_eq!(dollars_to_cents(Some(0.014)), 1);
        assert_eq!(dollars_to_cents(Some(0.026)), 3);
        assert_eq!(dollars_to_cents(None), 0);
        assert_eq!(dollars_to_cents(Some(0.0)), 0);
        assert_eq!(dollars_to_cents(Some(f64::NAN)), 0);
    }

    #[test]
    fn error_body_extracts_message_and_numeric_code() {
        let body = r#"{"error":{"message":"no such model","code":404}}"#;
        let err = ApiErrorBody::extract(body);
        assert_eq!(err.describe(), "no such model (code: 404)");
    }

    #[test]
    fn http_error_maps_to_taxonomy() {
        let model = ModelId::new("openai/gpt-4o");
        assert!(matches!(
            map_http_error(401, 0, "{}", &model),
            ProviderError::Unauthorized
        ));
        assert!(matches!(
            map_http_error(429, 1500, "{}", &model),
            ProviderError::RateLimited {
                retry_after_ms: 1500
            }
        ));
        assert!(matches!(
            map_http_error(404, 0, "{}", &model),
            ProviderError::ModelNotAvailable(_)
        ));
        assert!(matches!(
            map_http_error(400, 0, r#"{"error":{"message":"bad"}}"#, &model),
            ProviderError::InvalidRequest(_)
        ));
        assert!(matches!(
            map_http_error(500, 0, "boom", &model),
            ProviderError::Upstream(_)
        ));
    }

    #[test]
    fn finish_reason_mapping() {
        assert!(matches!(
            map_finish_reason(Some("stop")),
            FinishReason::Stop
        ));
        assert!(matches!(map_finish_reason(None), FinishReason::Stop));
        assert!(matches!(
            map_finish_reason(Some("length")),
            FinishReason::MaxTokens
        ));
        assert!(matches!(
            map_finish_reason(Some("tool_calls")),
            FinishReason::ToolUse(_)
        ));
        assert!(matches!(
            map_finish_reason(Some("content_filter")),
            FinishReason::Error(_)
        ));
    }

    #[test]
    fn response_parsing_extracts_content_usage_and_cost() {
        let raw = serde_json::json!({
            "id": "gen-1",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "pong"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 11, "completion_tokens": 2, "total_tokens": 13, "cost": 0.0123}
        });
        let parsed: ChatCompletion = serde_json::from_value(raw.clone()).unwrap();
        let resp = parsed.into_completion(raw);
        assert_eq!(resp.content, "pong");
        assert!(matches!(resp.finish_reason, FinishReason::Stop));
        assert_eq!(resp.usage.tokens_in, 11);
        assert_eq!(resp.usage.tokens_out, 2);
        assert_eq!(resp.cost.tokens_in, 11);
        assert_eq!(resp.cost.tokens_out, 2);
        assert_eq!(resp.cost.cents, 1); // 0.0123 USD → 1.23¢ → 1¢
        assert!(resp.raw_provider_response.is_some());
    }

    #[test]
    fn response_without_usage_bills_zero() {
        let raw = serde_json::json!({
            "choices": [{"message": {"content": "ok"}, "finish_reason": "stop"}]
        });
        let parsed: ChatCompletion = serde_json::from_value(raw.clone()).unwrap();
        let resp = parsed.into_completion(raw);
        assert_eq!(resp.cost.cents, 0);
        assert_eq!(resp.usage, Usage::default());
    }

    #[test]
    fn empty_api_key_is_unauthorized() {
        // A provider built with an empty key short-circuits to Unauthorized
        // before any network call.
        let provider =
            OpenRouterProvider::new(OpenRouterConfig::new(""), ModelId::new("openai/gpt-4o"));
        let req = CompletionRequest::new(
            vec![ardur_runtime::ChatMessage::user("hi")],
            ModelId::new("openai/gpt-4o"),
            16,
        );
        let err = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(provider.complete(req))
            .unwrap_err();
        assert!(matches!(err, ProviderError::Unauthorized));
    }

    #[test]
    fn completions_url_trims_trailing_slash() {
        let cfg = OpenRouterConfig::new("k").base_url("http://localhost:1234/api/v1/");
        assert_eq!(
            cfg.completions_url(),
            "http://localhost:1234/api/v1/chat/completions"
        );
    }

    #[test]
    fn provider_id_is_openrouter() {
        let provider =
            OpenRouterProvider::new(OpenRouterConfig::new("k"), ModelId::new("openai/gpt-4o"));
        assert_eq!(provider.id(), ProviderId("openrouter".to_string()));
        assert!(!provider.supports_streaming());
    }
}
