//! ardur-provider-openai-compat — a generic OpenAI-compatible HTTP backend
//! (§12.5).
//!
//! The provider targets services that accept `POST /chat/completions` using the
//! OpenAI Chat Completions dialect. It defaults to OpenAI proper
//! (`https://api.openai.com/v1`) and can be pointed at compatible services such
//! as Groq, Together, Fireworks, DeepInfra, or a loopback vLLM deployment. The
//! model string is opaque to this layer and passes through unchanged; the
//! upstream endpoint validates it.
//!
//! # Phase 1 (this crate)
//!
//! - [`OpenAiCompatProvider`] — the backend. [`OpenAiCompatProvider::new`] /
//!   [`OpenAiCompatProvider::from_env`] build the live HTTP path.
//! - [`OpenAiCompatConfig`] — the connection config (api key, base URL, the
//!   request timeout), built with a small builder.
//! - The [`Provider::complete`] impl translates the runtime's
//!   [`CompletionRequest`] into the OpenAI chat-completions request body and the
//!   OpenAI-shaped response back into a [`CompletionResponse`]. Token usage is
//!   mapped from `usage`; cost is `0` unless an OpenAI-compatible gateway emits a
//!   non-standard `usage.cost` dollar field.
//!
//! # Phase 2b — streaming
//!
//! [`OpenAiCompatProvider::stream_chat`] runs a completion over the
//! OpenAI-compatible SSE stream (`stream: true`), yielding an
//! [`OpenAiCompatChunk`] feed: content deltas, incremental tool-call fragments
//! (reassembled by a [`ToolCallAccumulator`]), a terminal [`OpenAiCompatChunk::Done`]
//! finish reason, and the final token [`Usage`]. The streamed `Provider::complete`
//! path is unchanged (it stays `stream: false`).
//!
//! # §3.X — uniform `Provider::stream`
//!
//! [`Provider::stream`] overrides the trait default to expose that same SSE feed
//! as shared [`StreamEvent`](ardur_provider_runtime::StreamEvent)s, adapting
//! OpenAI's index-keyed tool-call deltas onto the shared id-keyed
//! `ToolCallStart` / `ToolCallDelta` events. [`Provider::supports_streaming`] is
//! therefore `true`.
//!
//! § 6.0 added tool use: the request advertises `tools`, an assistant turn that
//! requested tools replays its `tool_calls`, a [`Role::Tool`] result becomes a
//! `tool` message, and a `tool_calls` finish reason decodes into
//! [`FinishReason::ToolUse`] with the parsed calls.
//!
//! [`ModelId`]: ardur_provider_runtime::ModelId
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fmt;
use std::time::Duration;

use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, ModelId, Provider, ProviderError,
    ProviderStream, RateCard, ToolCall, Usage,
};
use ardur_runtime::{ChatMessage, CostTuple, ProviderId, Role};
use async_trait::async_trait;
use futures::Stream;
use serde::Deserialize;

mod streaming;
pub use streaming::{OpenAiCompatChunk, ToolCallAccumulator, ToolCallDelta};

/// The registry key this backend answers to.
const PROVIDER_ID: &str = "openai-compat";
/// OpenAI's Chat Completions API base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// The default per-request timeout.
const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// The environment variable the API key is read from by
/// [`OpenAiCompatConfig::from_env`] / [`OpenAiCompatProvider::from_env`].
pub const API_KEY_ENV: &str = "OPENAI_COMPAT_API_KEY";
/// Fallback API key env for OpenAI proper.
pub const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";
/// Optional env override for the compatible API base URL.
pub const BASE_URL_ENV: &str = "OPENAI_COMPAT_BASE_URL";
/// Optional env override for the request timeout in whole seconds.
pub const TIMEOUT_SECS_ENV: &str = "OPENAI_COMPAT_TIMEOUT_SECS";

/// How an [`OpenAiCompatProvider`] connects to the gateway.
///
/// Build it from an API key with [`OpenAiCompatConfig::new`] (or
/// [`OpenAiCompatConfig::from_env`]) and tune the optional fields with the
/// builder methods; every field but the key has a sensible default.
#[derive(Clone)]
pub struct OpenAiCompatConfig {
    api_key: String,
    base_url: String,
    request_timeout: Duration,
}

impl fmt::Debug for OpenAiCompatConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiCompatConfig")
            .field("api_key", &redacted_present(&self.api_key))
            .field("base_url", &self.base_url)
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

fn redacted_present(value: &str) -> &'static str {
    if value.is_empty() {
        "<unset>"
    } else {
        "<redacted>"
    }
}

impl OpenAiCompatConfig {
    /// A config bound to `api_key`, with the default base URL, attribution
    /// headers, and request timeout.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            request_timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }

    /// Read config from environment.
    ///
    /// `OPENAI_COMPAT_API_KEY` is preferred; `OPENAI_API_KEY` is accepted as a
    /// fallback for OpenAI proper. `OPENAI_COMPAT_BASE_URL` defaults to
    /// `https://api.openai.com/v1` and must be HTTPS unless it targets loopback
    /// HTTP for local development. Secret values are never logged or exposed.
    ///
    /// Returns [`ProviderError::Unauthorized`] when the variable is unset or
    /// empty — the "missing API key" config error.
    pub fn from_env() -> Result<Self, ProviderError> {
        let Some(key) = first_nonempty_env([API_KEY_ENV, OPENAI_API_KEY_ENV]) else {
            return Err(ProviderError::Unauthorized);
        };

        let mut config = Self::new(key);
        if let Some(base_url) = nonempty_env(BASE_URL_ENV) {
            validate_base_url(&base_url)?;
            config = config.base_url(base_url);
        }
        if let Some(raw_timeout) = nonempty_env(TIMEOUT_SECS_ENV) {
            let secs = raw_timeout.trim().parse::<u64>().map_err(|_| {
                ProviderError::InvalidRequest(format!(
                    "{TIMEOUT_SECS_ENV} must be a positive integer number of seconds"
                ))
            })?;
            if secs == 0 {
                return Err(ProviderError::InvalidRequest(format!(
                    "{TIMEOUT_SECS_ENV} must be greater than zero"
                )));
            }
            config = config.request_timeout(Duration::from_secs(secs));
        }

        Ok(config)
    }

    /// Override the API base URL (e.g. to point at a mock server in tests).
    /// A trailing slash is trimmed so the `/chat/completions` join is stable.
    #[must_use]
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
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

/// The OpenAI-compatible provider.
///
/// Construct it with [`OpenAiCompatProvider::new`] (from an [`OpenAiCompatConfig`]
/// and a default model) or [`OpenAiCompatProvider::from_env`] (reading the API key
/// from `OPENAI_COMPAT_API_KEY` / `OPENAI_API_KEY`). The model on each
/// [`CompletionRequest`] selects which upstream model the compatible endpoint
/// routes to; `model_id` is only the default the runtime stamps onto a request.
pub struct OpenAiCompatProvider {
    config: OpenAiCompatConfig,
    model_id: ModelId,
    rate_card: RateCard,
    client: reqwest::Client,
}

impl OpenAiCompatProvider {
    /// Build a live provider from `config` with a default `model_id`.
    #[must_use]
    pub fn new(config: OpenAiCompatConfig, model_id: ModelId) -> Self {
        Self {
            config,
            model_id,
            rate_card: openai_compat_passthrough_rate_card(),
            client: reqwest::Client::new(),
        }
    }

    /// Build a live provider with a default `model_id`, reading the API key from
    /// `OPENAI_COMPAT_API_KEY` or `OPENAI_API_KEY`.
    ///
    /// Returns [`ProviderError::Unauthorized`] if the variable is unset or empty.
    pub fn from_env(model_id: ModelId) -> Result<Self, ProviderError> {
        Ok(Self::new(OpenAiCompatConfig::from_env()?, model_id))
    }

    /// The model this provider defaults completions to.
    #[must_use]
    pub fn model_id(&self) -> &ModelId {
        &self.model_id
    }

    /// Run a completion over an OpenAI-compatible SSE stream, returning a stream
    /// of [`OpenAiCompatChunk`] events.
    ///
    /// The request is sent with `stream: true` and
    /// `stream_options.include_usage: true`, so the upstream emits the
    /// OpenAI-compatible `data: {…}\n\n` event feed and a final usage chunk. The
    /// returned stream interleaves content deltas, incremental tool-call
    /// fragments (also stitched into the assembled calls delivered on
    /// [`OpenAiCompatChunk::Done`]), the finish reason, and the final
    /// [`OpenAiCompatChunk::Usage`]; it ends at the upstream `[DONE]` marker.
    ///
    /// A failure to connect, an empty API key, or a non-2xx status surfaces as a
    /// single terminal `Err` item rather than panicking — callers handle errors
    /// uniformly by inspecting each yielded `Result`. **Cancellation is by
    /// drop**: dropping the returned stream drops the underlying HTTP body and
    /// closes the connection.
    ///
    /// This is the OpenAI-compatible chunk surface; the uniform
    /// `Provider::stream` trait method adapts it to shared events.
    pub async fn stream_chat(
        &self,
        req: CompletionRequest,
    ) -> impl Stream<Item = Result<OpenAiCompatChunk, ProviderError>> + Send {
        if self.config.api_key.is_empty() {
            return error_stream(ProviderError::Unauthorized);
        }
        if let Err(err) = validate_base_url(&self.config.base_url) {
            return error_stream(err);
        }

        let body = build_stream_request_body(&req);
        let send = self
            .client
            .post(self.config.completions_url())
            .bearer_auth(&self.config.api_key)
            .timeout(self.config.request_timeout)
            .json(&body)
            .send()
            .await;

        let resp = match send {
            Ok(resp) => resp,
            Err(e) => return error_stream(ProviderError::NetworkFailure(e.to_string())),
        };

        let status = resp.status();
        if status.is_success() {
            return Box::pin(streaming::into_chunk_stream(resp));
        }

        // Drain the error body and map it through the same taxonomy as `complete`.
        let retry_after_ms = parse_retry_after_ms(resp.headers());
        let code = status.as_u16();
        let body = resp.text().await.unwrap_or_default();
        error_stream(map_http_error(code, retry_after_ms, &body, &req.model))
    }
}

/// A terminal one-item stream carrying a connect-time error, boxed so it shares
/// the concrete return type of [`OpenAiCompatProvider::stream_chat`]'s success
/// path.
fn error_stream(
    err: ProviderError,
) -> std::pin::Pin<Box<dyn Stream<Item = Result<OpenAiCompatChunk, ProviderError>> + Send>> {
    Box::pin(futures::stream::once(async move { Err(err) }))
}

/// Return the first configured, non-empty environment variable from `keys`.
fn first_nonempty_env<const N: usize>(keys: [&str; N]) -> Option<String> {
    keys.into_iter().find_map(nonempty_env)
}

/// Return an environment variable only when it contains non-whitespace text.
fn nonempty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Validate an env-sourced base URL before a secret-bearing HTTP client uses it.
///
/// HTTPS is always allowed. Plain HTTP is allowed only for loopback hosts so
/// local vLLM-style development remains possible without sending bearer tokens
/// over cleartext networks.
fn validate_base_url(base_url: &str) -> Result<(), ProviderError> {
    let parsed = url::Url::parse(base_url).map_err(|e| {
        ProviderError::InvalidRequest(format!("{BASE_URL_ENV} is not a valid URL: {e}"))
    })?;

    match parsed.scheme() {
        "https" => Ok(()),
        "http" if is_loopback_host(&parsed) => Ok(()),
        "http" => Err(ProviderError::InvalidRequest(format!(
            "{BASE_URL_ENV} must use https unless it points at localhost/loopback"
        ))),
        other => Err(ProviderError::InvalidRequest(format!(
            "{BASE_URL_ENV} must use https or loopback http, got scheme {other:?}"
        ))),
    }
}

/// Whether a URL host is local loopback.
fn is_loopback_host(url: &url::Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("::1") | Some("[::1]")
    )
}

#[async_trait]
impl Provider for OpenAiCompatProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        if self.config.api_key.is_empty() {
            return Err(ProviderError::Unauthorized);
        }
        validate_base_url(&self.config.base_url)?;

        let body = build_request_body(&req);

        let resp = self
            .client
            .post(self.config.completions_url())
            .bearer_auth(&self.config.api_key)
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

    /// Stream the completion as shared [`StreamEvent`](ardur_provider_runtime::StreamEvent)s
    /// (§3.X) — the uniform streaming surface, layered over the OpenAI-compatible
    /// [`stream_chat`](Self::stream_chat) chunk feed.
    ///
    /// The SSE handshake mirrors [`complete`](Self::complete): an empty key, a
    /// connect failure, or a non-2xx status is the `Err` of the returned `Result`
    /// (resolved before any event yields), and a mid-stream transport error is an
    /// `Err` item. On success the byte feed is decoded into [`OpenAiCompatChunk`]s
    /// and adapted by [`streaming::into_provider_events`]: content/usage/finish
    /// pass straight through, and OpenAI's index-keyed tool-call deltas are
    /// remapped onto the shared id-keyed [`ToolCallStart`]/[`ToolCallDelta`]
    /// events. Cancellation is by drop.
    ///
    /// [`ToolCallStart`]: ardur_provider_runtime::StreamEvent::ToolCallStart
    /// [`ToolCallDelta`]: ardur_provider_runtime::StreamEvent::ToolCallDelta
    async fn stream(&self, req: CompletionRequest) -> Result<ProviderStream, ProviderError> {
        if self.config.api_key.is_empty() {
            return Err(ProviderError::Unauthorized);
        }
        validate_base_url(&self.config.base_url)?;

        let body = build_stream_request_body(&req);
        let resp = self
            .client
            .post(self.config.completions_url())
            .bearer_auth(&self.config.api_key)
            .timeout(self.config.request_timeout)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::NetworkFailure(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            // Drain the error body and map it through the same taxonomy as `complete`.
            let retry_after_ms = parse_retry_after_ms(resp.headers());
            let code = status.as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(map_http_error(code, retry_after_ms, &body, &req.model));
        }

        let chunks = streaming::into_chunk_stream(resp);
        Ok(Box::pin(streaming::into_provider_events(chunks)))
    }

    fn id(&self) -> ProviderId {
        ProviderId(PROVIDER_ID.to_string())
    }

    fn supports_streaming(&self) -> bool {
        // §3.X: the uniform `Provider::stream` trait method is live.
        true
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// A zeroed "passthrough" rate card.
///
/// A zeroed passthrough rate card.
///
/// OpenAI-compatible providers do not expose one common pricing catalog. The
/// generic adapter therefore trusts token usage from the response and uses `0`
/// cents unless a gateway emits the non-standard `usage.cost` dollar field. The
/// card exists only to satisfy [`Provider::rate_card`].
fn openai_compat_passthrough_rate_card() -> RateCard {
    RateCard {
        version_id: "openai-compat-passthrough-v1".to_string(),
        cents_per_1k_input: 0.0,
        cents_per_1k_output: 0.0,
        cents_per_request: 0.0,
    }
}

/// Serialize a [`CompletionRequest`] into the OpenAI chat-completions body.
///
/// Unlike the Anthropic Messages API, the OpenAI shape keeps `system` turns
/// inline in the `messages` array. An assistant turn that requested tools
/// replays its `tool_calls` (arguments re-encoded as the JSON *string* OpenAI
/// expects), and a [`Role::Tool`] result becomes a `tool` message keyed by
/// `tool_call_id` (§6.0). `stop`, `tools` are omitted when empty. `stream` is
/// `false` — the non-streaming [`Provider::complete`] path (see
/// [`build_stream_request_body`] for the §3.2b streaming body).
fn build_request_body(req: &CompletionRequest) -> serde_json::Value {
    request_body(req, false)
}

/// The streaming request body (§3.2b): identical to [`build_request_body`] but
/// `stream: true` with `stream_options.include_usage: true`, so compatible
/// endpoints that implement OpenAI usage streaming emit a final usage chunk
/// before `[DONE]`.
fn build_stream_request_body(req: &CompletionRequest) -> serde_json::Value {
    let mut body = request_body(req, true);
    body.as_object_mut()
        .expect("json! object literal is always a map")
        .insert(
            "stream_options".to_string(),
            serde_json::json!({ "include_usage": true }),
        );
    body
}

/// Shared body builder for both the non-streaming and streaming paths; `stream`
/// pins the OpenAI `stream` flag.
fn request_body(req: &CompletionRequest, stream: bool) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = req.messages.iter().map(openai_compat_message).collect();

    let mut body = serde_json::json!({
        "model": req.model.0,
        "messages": messages,
        "max_tokens": req.max_tokens,
        "temperature": req.temperature,
        "stream": stream,
    });
    let map = body
        .as_object_mut()
        .expect("json! object literal is always a map");
    if !req.stop_sequences.is_empty() {
        map.insert("stop".to_string(), serde_json::json!(req.stop_sequences));
    }
    if !req.tools.is_empty() {
        let tools: Vec<serde_json::Value> = req
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    },
                })
            })
            .collect();
        map.insert("tools".to_string(), serde_json::Value::Array(tools));
    }
    body
}

/// Serialize one [`ChatMessage`] into an OpenAI `messages` entry. A plain turn
/// is `{role, content}`; an assistant turn carrying tool calls adds a
/// `tool_calls` array; a [`Role::Tool`] result is a `tool` message keyed by the
/// `tool_call_id` it answers.
fn openai_compat_message(m: &ChatMessage) -> serde_json::Value {
    match m.role {
        Role::Tool => serde_json::json!({
            "role": "tool",
            "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
            "content": m.content,
        }),
        Role::Assistant if !m.tool_calls.is_empty() => {
            let tool_calls: Vec<serde_json::Value> = m
                .tool_calls
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "id": c.id,
                        "type": "function",
                        "function": {
                            "name": c.name,
                            // OpenAI carries the arguments as a JSON-encoded string.
                            "arguments": serde_json::to_string(&c.arguments).unwrap_or_default(),
                        },
                    })
                })
                .collect();
            serde_json::json!({
                "role": "assistant",
                "content": m.content,
                "tool_calls": tool_calls,
            })
        }
        _ => serde_json::json!({
            "role": role_str(m.role),
            "content": m.content,
        }),
    }
}

/// The OpenAI/`messages` role wire string for a [`Role`].
fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
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

/// Map a non-2xx OpenAI-compatible response onto the crate's [`ProviderError`]
/// taxonomy. The common error body is `{ "error": { "message", "code", … } }`;
/// its message and code, when present, are surfaced in the mapped error.
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

/// The common `{ "error": { … } }` envelope returned on a failed call.
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
            (None, Some(c)) => format!("openai-compatible error (code: {c})"),
            (None, None) => "openai-compatible error with no message".to_string(),
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
/// tool-call-only turn carries `null`; `tool_calls` carries the requested calls
/// (§6.0).
#[derive(Default, Deserialize)]
struct Message {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ApiToolCall>,
}

/// One entry of an assistant message's `tool_calls` array (OpenAI shape).
#[derive(Deserialize)]
struct ApiToolCall {
    #[serde(default)]
    id: String,
    function: ApiToolFunction,
}

/// The `function` object inside an [`ApiToolCall`]: the tool name and its
/// arguments as a JSON-encoded string.
#[derive(Deserialize)]
struct ApiToolFunction {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
}

/// The `usage` object. `cost` is a non-standard optional dollar cost emitted by
/// some OpenAI-compatible gateways; OpenAI proper omits it.
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
    /// from a non-standard `usage.cost` dollar field when a gateway provides it,
    /// otherwise `0`.
    fn into_completion(self, raw: serde_json::Value) -> CompletionResponse {
        let (content, tool_calls, finish_raw) = match self.choices.into_iter().next() {
            Some(choice) => {
                let content = choice.message.content.clone().unwrap_or_default();
                // OpenAI carries each call's arguments as a JSON-encoded string;
                // decode it back to a value, defaulting to null if it is empty
                // or malformed.
                let tool_calls: Vec<ToolCall> = choice
                    .message
                    .tool_calls
                    .iter()
                    .map(|tc| ToolCall {
                        id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        arguments: serde_json::from_str(&tc.function.arguments)
                            .unwrap_or(serde_json::Value::Null),
                    })
                    .collect();
                (content, tool_calls, choice.finish_reason)
            }
            None => (String::new(), Vec::new(), None),
        };
        let finish_reason = map_finish_reason(finish_raw.as_deref(), tool_calls);

        let (usage, cents) = match self.usage {
            Some(u) => {
                let usage = Usage {
                    tokens_in: u.prompt_tokens,
                    tokens_out: u.completion_tokens,
                    cost_cents: None,
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

/// Map the OpenAI `finish_reason` onto the crate's [`FinishReason`], carrying the
/// already-decoded `tool_calls` into the `tool_calls` arm. A missing reason
/// (`null`) is treated as a natural stop.
fn map_finish_reason(reason: Option<&str>, tool_calls: Vec<ToolCall>) -> FinishReason {
    match reason {
        Some("stop") | None => FinishReason::Stop,
        Some("length") => FinishReason::MaxTokens,
        Some("tool_calls") => FinishReason::ToolUse(tool_calls),
        Some("content_filter") => {
            FinishReason::Error("generation halted by content filter".to_string())
        }
        Some(other) => FinishReason::Error(format!("unknown finish_reason: {other}")),
    }
}

/// Convert an optional dollar cost into whole US cents, rounding to the nearest
/// cent. Missing cost (OpenAI proper and many compatible endpoints) is `0`.
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
            map_finish_reason(Some("stop"), Vec::new()),
            FinishReason::Stop
        ));
        assert!(matches!(
            map_finish_reason(None, Vec::new()),
            FinishReason::Stop
        ));
        assert!(matches!(
            map_finish_reason(Some("length"), Vec::new()),
            FinishReason::MaxTokens
        ));
        // The decoded calls flow straight into the ToolUse variant.
        let calls = vec![ToolCall {
            id: "call_1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({"msg": "hi"}),
        }];
        assert!(matches!(
            map_finish_reason(Some("tool_calls"), calls),
            FinishReason::ToolUse(c) if c.len() == 1 && c[0].name == "echo"
        ));
        assert!(matches!(
            map_finish_reason(Some("content_filter"), Vec::new()),
            FinishReason::Error(_)
        ));
    }

    #[test]
    fn response_parsing_decodes_tool_calls() {
        let raw = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {"name": "echo", "arguments": "{\"msg\":\"hi\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let parsed: ChatCompletion = serde_json::from_value(raw.clone()).unwrap();
        let resp = parsed.into_completion(raw);
        match resp.finish_reason {
            FinishReason::ToolUse(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "call_abc");
                assert_eq!(calls[0].name, "echo");
                assert_eq!(calls[0].arguments, serde_json::json!({"msg": "hi"}));
            }
            other => unreachable!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn request_body_serializes_tools_and_tool_messages() {
        use ardur_provider_runtime::ToolDef;
        use ardur_runtime::ChatMessage;
        let calls = vec![ToolCall {
            id: "call_1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({"msg": "hi"}),
        }];
        let mut req = CompletionRequest::new(
            vec![
                ChatMessage::user("call echo"),
                ChatMessage::assistant_tool_calls("", calls),
                ChatMessage::tool_result("call_1", "{\"msg\":\"hi\"}"),
            ],
            ModelId::new("openai/gpt-4o"),
            64,
        );
        req.tools = vec![ToolDef {
            name: "echo".to_string(),
            description: "echoes".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let body = build_request_body(&req);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "echo");
        assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            body["messages"][1]["tool_calls"][0]["function"]["name"],
            "echo"
        );
        assert_eq!(body["messages"][2]["role"], "tool");
        assert_eq!(body["messages"][2]["tool_call_id"], "call_1");
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
            OpenAiCompatProvider::new(OpenAiCompatConfig::new(""), ModelId::new("openai/gpt-4o"));
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
        let cfg = OpenAiCompatConfig::new("k").base_url("http://localhost:1234/api/v1/");
        assert_eq!(
            cfg.completions_url(),
            "http://localhost:1234/api/v1/chat/completions"
        );
    }

    #[test]
    fn debug_redacts_api_key() {
        let cfg = OpenAiCompatConfig::new("DUMMY_KEY_FOR_TESTING_ONLY");
        let rendered = format!("{cfg:?}");
        assert!(
            !rendered.contains("DUMMY_KEY_FOR_TESTING_ONLY"),
            "{rendered}"
        );
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn provider_id_is_openai_compat() {
        let provider =
            OpenAiCompatProvider::new(OpenAiCompatConfig::new("k"), ModelId::new("openai/gpt-4o"));
        assert_eq!(provider.id(), ProviderId("openai-compat".to_string()));
        assert!(provider.supports_streaming());
    }

    #[test]
    fn env_base_url_policy_requires_https_or_loopback_http() {
        assert!(validate_base_url("https://api.openai.com/v1").is_ok());
        assert!(validate_base_url("http://localhost:8000/v1").is_ok());
        assert!(validate_base_url("http://127.0.0.1:8000/v1").is_ok());
        assert!(validate_base_url("http://[::1]:8000/v1").is_ok());
        assert!(matches!(
            validate_base_url("http://example.com/v1"),
            Err(ProviderError::InvalidRequest(_))
        ));
    }
}
