//! ardur-provider-ollama — the [Ollama] local + cloud model backend (§3.3).
//!
//! Ollama runs open models either **locally** — a daemon on
//! `http://localhost:11434` that needs no auth — or in Ollama's **hosted
//! cloud** at `https://ollama.com`, which authenticates with a Bearer API key.
//! Both expose the same native `POST /api/chat` endpoint, so a single backend
//! covers both: the only difference is the base URL and whether a key is sent.
//! This crate implements the §3.0 [`Provider`] trait against that endpoint, so
//! the runtime can dispatch a turn to any pulled model by passing its name
//! through the request's [`ModelId`] — e.g. `llama3.2`, `qwen2.5`,
//! `mistral`, `gpt-oss:20b`. The model string is opaque to this layer and
//! passes through unchanged; Ollama validates it against what is installed (or,
//! for the cloud, what it hosts).
//!
//! # Phase 1 (this crate)
//!
//! - [`OllamaProvider`] — the backend. [`OllamaProvider::new`] /
//!   [`OllamaProvider::from_env`] build the live HTTP path.
//! - [`OllamaConfig`] — the connection config (base URL, an optional API key
//!   for the cloud, request timeout, default model), built with a small
//!   builder. With no key it talks to a local daemon; with a key it Bearer-auths
//!   the cloud.
//! - The [`Provider::complete`] impl translates the runtime's
//!   [`CompletionRequest`] into Ollama's `/api/chat` request body and the
//!   response back into a [`CompletionResponse`]. Token counts come from
//!   `prompt_eval_count` / `eval_count`; Ollama bills no dollar cost, so the
//!   call is always priced at `0` cents.
//!
//! # Not in Phase 1
//!
//! - **Streaming** — every request sends `stream: false`;
//!   [`Provider::supports_streaming`] is `false`. (Phase 2.)
//! - **Tool-call parsing** — the message's `tool_calls` field is not decoded
//!   yet. (Phase 2.)
//!
//! [Ollama]: https://ollama.com
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
const PROVIDER_ID: &str = "ollama";
/// The default base URL — a local Ollama daemon, which needs no auth.
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434";
/// Ollama's hosted-cloud base URL — used as the default when an API key is
/// present but no explicit base URL was set.
pub const CLOUD_BASE_URL: &str = "https://ollama.com";
/// The model a config defaults to when none is set explicitly.
pub const DEFAULT_MODEL: &str = "llama3.2";
/// The default per-request timeout.
const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// The environment variable the base URL is read from by
/// [`OllamaConfig::from_env`] / [`OllamaProvider::from_env`].
pub const BASE_URL_ENV: &str = "OLLAMA_BASE_URL";
/// The environment variable the (optional) cloud API key is read from.
pub const API_KEY_ENV: &str = "OLLAMA_API_KEY";

/// How an [`OllamaProvider`] connects to Ollama.
///
/// Build it with [`OllamaConfig::new`] (a local, no-auth daemon) or
/// [`OllamaConfig::from_env`], then tune the optional fields with the builder
/// methods. Set an [`api_key`](OllamaConfig::api_key) to talk to the hosted
/// cloud instead of a local daemon; every field has a sensible default.
#[derive(Clone, Debug)]
pub struct OllamaConfig {
    base_url: String,
    api_key: Option<String>,
    request_timeout: Duration,
    default_model: ModelId,
}

impl OllamaConfig {
    /// A config for a local Ollama daemon: the default `localhost:11434` base
    /// URL, no API key, the default request timeout, and the default model.
    pub fn new() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: None,
            request_timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            default_model: ModelId::new(DEFAULT_MODEL),
        }
    }

    /// Read the connection config from the environment.
    ///
    /// - [`API_KEY_ENV`] (`OLLAMA_API_KEY`) sets the (optional) cloud key; an
    ///   unset or empty variable leaves it `None` (local, no auth).
    /// - [`BASE_URL_ENV`] (`OLLAMA_BASE_URL`) sets the base URL explicitly. When
    ///   it is unset, the base URL defaults to [`CLOUD_BASE_URL`] if a key was
    ///   found (the cloud) and [`DEFAULT_BASE_URL`] otherwise (a local daemon).
    ///
    /// Never fails — unlike a cloud-only provider, a local Ollama needs no
    /// credentials, so there is no "missing key" error.
    pub fn from_env() -> Self {
        let api_key = match std::env::var(API_KEY_ENV) {
            Ok(key) if !key.is_empty() => Some(key),
            _ => None,
        };
        let base_url = match std::env::var(BASE_URL_ENV) {
            Ok(url) if !url.is_empty() => url,
            // A key with no explicit URL means "use the cloud"; otherwise local.
            _ if api_key.is_some() => CLOUD_BASE_URL.to_string(),
            _ => DEFAULT_BASE_URL.to_string(),
        };
        Self {
            base_url,
            api_key,
            request_timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            default_model: ModelId::new(DEFAULT_MODEL),
        }
    }

    /// Override the API base URL (e.g. [`CLOUD_BASE_URL`], or a mock server in
    /// tests). A trailing slash is trimmed so the `/api/chat` join is stable.
    #[must_use]
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Set the cloud API key, which is sent as a `Bearer` token. Present a key
    /// to talk to the hosted cloud; leave it unset for a local daemon.
    #[must_use]
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Override the model a built provider defaults completions to.
    #[must_use]
    pub fn default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = ModelId::new(model);
        self
    }

    /// Override the per-request timeout.
    #[must_use]
    pub fn request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    /// The `/api/chat` endpoint URL for this config's base.
    fn chat_url(&self) -> String {
        format!("{}/api/chat", self.base_url.trim_end_matches('/'))
    }
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// The Ollama provider.
///
/// Construct it with [`OllamaProvider::new`] (from an [`OllamaConfig`]) or
/// [`OllamaProvider::from_env`] (reading the base URL + optional key from the
/// environment). The model on each [`CompletionRequest`] selects which Ollama
/// model runs the turn; the config's default model is only the default the
/// runtime stamps onto a request.
pub struct OllamaProvider {
    config: OllamaConfig,
    rate_card: RateCard,
    client: reqwest::Client,
}

impl OllamaProvider {
    /// Build a live provider from `config`.
    #[must_use]
    pub fn new(config: OllamaConfig) -> Self {
        Self {
            config,
            rate_card: ollama_zero_rate_card(),
            client: reqwest::Client::new(),
        }
    }

    /// Build a live provider with the connection config read from the
    /// environment (see [`OllamaConfig::from_env`]). Never fails — a local
    /// Ollama needs no credentials.
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(OllamaConfig::from_env())
    }

    /// The model this provider defaults completions to.
    #[must_use]
    pub fn model_id(&self) -> &ModelId {
        &self.config.default_model
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let body = build_request_body(&req);

        let mut request = self
            .client
            .post(self.config.chat_url())
            .timeout(self.config.request_timeout)
            .json(&body);
        // Bearer auth only for the cloud; a local daemon takes no credentials.
        if let Some(key) = &self.config.api_key {
            request = request.bearer_auth(key);
        }

        let resp = request
            .send()
            .await
            .map_err(|e| map_send_error(e, &self.config.base_url))?;

        let status = resp.status();
        if status.is_success() {
            let value: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| ProviderError::Upstream(format!("decoding response body: {e}")))?;
            let parsed: ChatResponse = serde_json::from_value(value.clone())
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
        // Phase 2: flip to `true` once the NDJSON streaming path lands.
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// A zeroed rate card.
///
/// Ollama reports no dollar cost — a local daemon is free and the cloud bills
/// out-of-band — so every call is priced at `0` cents and this card is never
/// consulted for pricing. It exists only to satisfy [`Provider::rate_card`].
fn ollama_zero_rate_card() -> RateCard {
    RateCard {
        version_id: "ollama-zero-v1".to_string(),
        cents_per_1k_input: 0.0,
        cents_per_1k_output: 0.0,
        cents_per_request: 0.0,
    }
}

/// Serialize a [`CompletionRequest`] into Ollama's `/api/chat` request body.
///
/// Ollama keeps `system` turns inline in the `messages` array (like the OpenAI
/// shape), so every role maps one-to-one. Sampling knobs live under `options`:
/// `temperature` and `num_predict` (Ollama's name for the output-token cap),
/// plus `stop` when the request carries stop sequences. `stream` is pinned
/// `false` — Phase 1 is non-streaming.
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

    let mut options = serde_json::json!({
        "temperature": req.temperature,
        "num_predict": req.max_tokens,
    });
    if !req.stop_sequences.is_empty() {
        options
            .as_object_mut()
            .expect("json! object literal is always a map")
            .insert("stop".to_string(), serde_json::json!(req.stop_sequences));
    }

    serde_json::json!({
        "model": req.model.0,
        "messages": messages,
        "stream": false,
        "options": options,
    })
}

/// The `messages` role wire string for a [`Role`].
fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        // §6.0: Ollama's chat API takes tool results under the `tool` role. This
        // provider does not advertise tools in Phase 1, but a tool transcript
        // still serializes correctly if one is replayed through it.
        Role::Tool => "tool",
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

/// Map a reqwest send failure onto the crate's error taxonomy. A connection
/// refusal is the common "the daemon isn't running" case, so it surfaces as an
/// [`Upstream`](ProviderError::Upstream) error with a hint pointing at the base
/// URL; any other transport failure is a [`NetworkFailure`].
fn map_send_error(e: reqwest::Error, base_url: &str) -> ProviderError {
    if e.is_connect() {
        ProviderError::Upstream(format!(
            "could not connect to Ollama at {base_url} — is the daemon running? \
             (start it with `ollama serve`, or set OLLAMA_BASE_URL / an API key for the cloud): {e}"
        ))
    } else {
        ProviderError::NetworkFailure(e.to_string())
    }
}

/// Map a non-2xx Ollama response onto the crate's [`ProviderError`] taxonomy.
/// Ollama's error body is `{ "error": "<message>" }` (a plain string), whose
/// message is surfaced in the mapped error.
fn map_http_error(code: u16, retry_after_ms: u64, body: &str, model: &ModelId) -> ProviderError {
    let message = extract_error_message(body);
    match code {
        401 | 403 => ProviderError::Unauthorized,
        429 => ProviderError::RateLimited { retry_after_ms },
        400 => ProviderError::InvalidRequest(message),
        404 => ProviderError::ModelNotAvailable(model.clone()),
        _ => ProviderError::Upstream(format!("HTTP {code}: {message}")),
    }
}

/// Pull the `error` string out of an Ollama error body, falling back to a
/// generic note when the body is empty or not the expected shape.
fn extract_error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .as_ref()
        .and_then(|v| v["error"].as_str().map(str::to_string))
        .unwrap_or_else(|| "ollama error with no message".to_string())
}

/// The subset of the Ollama `/api/chat` response this Phase-1 path reads.
#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    message: ResponseMessage,
    /// Why generation stopped — Ollama's `done_reason` (`"stop"`, `"length"`).
    #[serde(default)]
    done_reason: Option<String>,
    /// Tokens evaluated from the prompt → billed as input.
    #[serde(default)]
    prompt_eval_count: u32,
    /// Tokens generated in the response → billed as output.
    #[serde(default)]
    eval_count: u32,
}

/// The assistant message inside a [`ChatResponse`]. `content` is optional so a
/// tool-call-only turn (Phase 2) does not fail to decode.
#[derive(Default, Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: Option<String>,
}

impl ChatResponse {
    /// Fold the parsed response into a [`CompletionResponse`], retaining `raw`
    /// for audit. Token counts come from `prompt_eval_count` / `eval_count`;
    /// Ollama reports no dollar cost, so the call is always billed `0` cents.
    fn into_completion(self, raw: serde_json::Value) -> CompletionResponse {
        let content = self.message.content.unwrap_or_default();
        let finish_reason = map_finish_reason(self.done_reason.as_deref());

        let usage = Usage {
            tokens_in: self.prompt_eval_count,
            tokens_out: self.eval_count,
        };

        let cost = CostTuple {
            tokens_in: u64::from(usage.tokens_in),
            tokens_out: u64::from(usage.tokens_out),
            cents: 0,
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

/// Map Ollama's `done_reason` onto the crate's [`FinishReason`]. A missing
/// reason is treated as a natural stop.
fn map_finish_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("stop") | None => FinishReason::Stop,
        Some("length") => FinishReason::MaxTokens,
        Some(other) => FinishReason::Error(format!("unknown done_reason: {other}")),
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
            ModelId::new("llama3.2"),
            128,
        );
        req.temperature = 0.5;
        req.stop_sequences = vec!["STOP".to_string()];

        let body = build_request_body(&req);
        assert_eq!(body["model"], "llama3.2");
        assert_eq!(body["stream"], false);
        assert_eq!(body["options"]["num_predict"], 128);
        assert_eq!(body["options"]["temperature"], 0.5);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][2]["role"], "assistant");
        assert_eq!(body["options"]["stop"][0], "STOP");
    }

    #[test]
    fn request_body_omits_stop_when_empty() {
        use ardur_runtime::ChatMessage;
        let req =
            CompletionRequest::new(vec![ChatMessage::user("hi")], ModelId::new("llama3.2"), 16);
        let body = build_request_body(&req);
        assert!(
            body["options"].get("stop").is_none(),
            "no stop key when none requested"
        );
    }

    #[test]
    fn error_message_extracts_plain_string() {
        let body = r#"{"error":"model 'nope' not found, try pulling it first"}"#;
        assert_eq!(
            extract_error_message(body),
            "model 'nope' not found, try pulling it first"
        );
        assert_eq!(
            extract_error_message("not json"),
            "ollama error with no message"
        );
    }

    #[test]
    fn http_error_maps_to_taxonomy() {
        let model = ModelId::new("llama3.2");
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
            map_http_error(404, 0, r#"{"error":"not found"}"#, &model),
            ProviderError::ModelNotAvailable(_)
        ));
        assert!(matches!(
            map_http_error(400, 0, r#"{"error":"bad request"}"#, &model),
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
            map_finish_reason(Some("weird")),
            FinishReason::Error(_)
        ));
    }

    #[test]
    fn response_parsing_extracts_content_and_token_counts() {
        let raw = serde_json::json!({
            "model": "llama3.2",
            "message": {"role": "assistant", "content": "pong"},
            "done_reason": "stop",
            "done": true,
            "prompt_eval_count": 11,
            "eval_count": 2
        });
        let parsed: ChatResponse = serde_json::from_value(raw.clone()).unwrap();
        let resp = parsed.into_completion(raw);
        assert_eq!(resp.content, "pong");
        assert!(matches!(resp.finish_reason, FinishReason::Stop));
        assert_eq!(resp.usage.tokens_in, 11);
        assert_eq!(resp.usage.tokens_out, 2);
        assert_eq!(resp.cost.tokens_in, 11);
        assert_eq!(resp.cost.tokens_out, 2);
        // Ollama bills no dollar cost — always zero cents.
        assert_eq!(resp.cost.cents, 0);
        assert!(resp.raw_provider_response.is_some());
    }

    #[test]
    fn response_without_counts_bills_zero_and_defaults() {
        let raw = serde_json::json!({
            "message": {"content": "ok"},
            "done_reason": "stop"
        });
        let parsed: ChatResponse = serde_json::from_value(raw.clone()).unwrap();
        let resp = parsed.into_completion(raw);
        assert_eq!(resp.cost.cents, 0);
        assert_eq!(resp.usage, Usage::default());
    }

    #[test]
    fn chat_url_trims_trailing_slash() {
        let cfg = OllamaConfig::new().base_url("http://localhost:11434/");
        assert_eq!(cfg.chat_url(), "http://localhost:11434/api/chat");
    }

    #[test]
    fn local_config_has_no_api_key() {
        let cfg = OllamaConfig::new();
        assert!(cfg.api_key.is_none());
        assert_eq!(cfg.base_url, DEFAULT_BASE_URL);
    }

    #[test]
    fn cloud_config_carries_bearer_key() {
        let cfg = OllamaConfig::new()
            .base_url(CLOUD_BASE_URL)
            .api_key("sk-test");
        assert_eq!(cfg.api_key.as_deref(), Some("sk-test"));
        assert_eq!(cfg.base_url, CLOUD_BASE_URL);
    }

    #[test]
    fn provider_id_is_ollama_and_not_streaming() {
        let provider = OllamaProvider::new(OllamaConfig::new());
        assert_eq!(provider.id(), ProviderId("ollama".to_string()));
        assert!(!provider.supports_streaming());
        assert_eq!(provider.model_id().0, DEFAULT_MODEL);
    }
}
