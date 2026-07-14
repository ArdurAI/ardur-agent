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
//! # Phase 2 — NDJSON streaming (§3.4b)
//!
//! Ollama streams a completion as **newline-delimited JSON**: one JSON object
//! per generated chunk, each a partial token, terminated by a final object with
//! `"done": true` that carries the run's `prompt_eval_count` / `eval_count`.
//! Both `POST /api/chat` and `POST /api/generate` stream this way (chat puts the
//! token under `message.content`, generate under `response`), so a single
//! [`OllamaChatChunk`] decodes either.
//!
//! - [`OllamaProvider::stream_ndjson`] (chat) and
//!   [`OllamaProvider::stream_ndjson_generate`] (generate) return a concrete
//!   `impl Stream<Item = Result<OllamaChatChunk, ProviderError>>` — one item per
//!   NDJSON line, the upstream HTTP handshake / non-2xx already resolved before
//!   the stream yields.
//! - [`Provider::stream`] (§3.X) is the uniform streaming surface: it overrides
//!   the trait default to drive [`stream_ndjson`] under the hood and adapt each
//!   [`OllamaChatChunk`] into a shared
//!   [`StreamEvent`](ardur_provider_runtime::StreamEvent) — a
//!   [`ContentDelta`](ardur_provider_runtime::StreamEvent::ContentDelta) per
//!   token chunk, then a final
//!   [`Usage`](ardur_provider_runtime::StreamEvent::Usage) and terminal
//!   [`Finish`](ardur_provider_runtime::StreamEvent::Finish) folded from the
//!   `done` chunk's token counts and `done_reason`. (Ollama does not advertise
//!   tools yet, so no `ToolCall*` events are produced.)
//! - **Cancellation** is the natural drop of the returned stream: dropping it
//!   drops the underlying `reqwest` byte stream, which closes the connection and
//!   stops the generation upstream — no explicit abort handle needed.
//!
//! [`Provider::supports_streaming`] is `true` accordingly.
//!
//! # Not yet
//!
//! - **Tool-call parsing** — the message's `tool_calls` field is not decoded
//!   yet.
//!
//! [Ollama]: https://ollama.com
//! [`ModelId`]: ardur_provider_runtime::ModelId
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fmt;
use std::pin::Pin;
use std::time::Duration;

use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, ModelId, Provider, ProviderError,
    ProviderStream, RateCard, StreamEvent, Usage,
};
use ardur_resilience::{
    CircuitBreakerConfig, RetryPolicy,
    circuit_breaker::{CircuitBreaker, CircuitError},
    retry::retry_with_backoff,
};
use ardur_runtime::{CostTuple, ProviderId, Role};
use async_trait::async_trait;
use futures::stream::{Stream, StreamExt};
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

/// The boxed NDJSON chunk stream the streaming entry points return.
///
/// A `'static`, `Send` (so it crosses the async runtime's threads) and `Unpin`
/// (so callers drive it with [`StreamExt::next`] directly) trait object — one
/// item per newline-delimited JSON object. Naming the type concretely (rather
/// than an `impl Stream` that would capture `&self`'s lifetime) lets
/// [`Provider::stream`] box the chunk feed into a `'static` [`ProviderStream`].
pub type OllamaChunkStream =
    Pin<Box<dyn Stream<Item = Result<OllamaChatChunk, ProviderError>> + Send>>;
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
#[derive(Clone)]
pub struct OllamaConfig {
    base_url: String,
    api_key: Option<String>,
    request_timeout: Duration,
    default_model: ModelId,
    retry_policy: RetryPolicy,
    circuit_breaker: CircuitBreakerConfig,
}

impl fmt::Debug for OllamaConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OllamaConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &redacted_present(self.api_key.as_deref()))
            .field("request_timeout", &self.request_timeout)
            .field("default_model", &self.default_model)
            .finish()
    }
}

fn redacted_present(value: Option<&str>) -> &'static str {
    if value.is_some_and(|v| !v.is_empty()) {
        "<redacted>"
    } else {
        "<unset>"
    }
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
            retry_policy: RetryPolicy::default(),
            circuit_breaker: CircuitBreakerConfig::default(),
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
            retry_policy: RetryPolicy::default(),
            circuit_breaker: CircuitBreakerConfig::default(),
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

    /// Override the retry/backoff policy applied to transient failures
    /// (network errors, `429`, `5xx`). The default retries twice with
    /// exponential backoff + full jitter; pass [`RetryPolicy::none`] to
    /// disable retrying entirely.
    #[must_use]
    pub fn retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }

    /// Override the circuit breaker that trips after repeated failures and
    /// fails fast (without hitting the network) until it cools down.
    #[must_use]
    pub fn circuit_breaker(mut self, circuit_breaker: CircuitBreakerConfig) -> Self {
        self.circuit_breaker = circuit_breaker;
        self
    }

    /// The `/api/chat` endpoint URL for this config's base.
    fn chat_url(&self) -> String {
        format!("{}/api/chat", self.base_url.trim_end_matches('/'))
    }

    /// The `/api/generate` endpoint URL for this config's base.
    fn generate_url(&self) -> String {
        format!("{}/api/generate", self.base_url.trim_end_matches('/'))
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
    /// Shared across every call this provider makes — a run of failures on
    /// `complete` also trips the breaker the streaming entry points see,
    /// since they all hit the same daemon/cloud endpoint.
    breaker: CircuitBreaker,
}

impl OllamaProvider {
    /// Build a live provider from `config`.
    #[must_use]
    pub fn new(config: OllamaConfig) -> Self {
        let breaker = CircuitBreaker::new(config.circuit_breaker.clone());
        Self {
            config,
            rate_card: ollama_zero_rate_card(),
            client: reqwest::Client::new(),
            breaker,
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

    /// Stream a chat completion as NDJSON token chunks from `POST /api/chat`.
    ///
    /// Returns a concrete `impl Stream` yielding one [`OllamaChatChunk`] per
    /// newline-delimited JSON object Ollama emits — each a partial token, the
    /// last one carrying `done: true` and the run's final token counts. The
    /// upstream handshake and any non-2xx status are resolved *before* the stream
    /// is returned, so an `Err` here is a connect/HTTP failure (mapped exactly as
    /// [`Provider::complete`] maps it); errors *mid-stream* surface as an `Err`
    /// item. Drop the returned stream to cancel the generation (§3.4b): that
    /// drops the underlying byte stream and closes the connection.
    pub async fn stream_ndjson(
        &self,
        req: CompletionRequest,
    ) -> Result<OllamaChunkStream, ProviderError> {
        if self.config.api_key.is_some() {
            validate_base_url_for_bearer(&self.config.base_url)?;
        }
        let body = build_request_body(&req, true);
        self.open_ndjson(self.config.chat_url(), body, req.model)
            .await
    }

    /// Stream a completion as NDJSON token chunks from `POST /api/generate`.
    ///
    /// The same wire shape as [`stream_ndjson`](Self::stream_ndjson), but against
    /// the prompt-completion endpoint: the request's messages are flattened into
    /// a single `prompt`, and each chunk's token rides under `response` rather
    /// than `message.content` (both decoded by [`OllamaChatChunk`]).
    pub async fn stream_ndjson_generate(
        &self,
        req: CompletionRequest,
    ) -> Result<OllamaChunkStream, ProviderError> {
        if self.config.api_key.is_some() {
            validate_base_url_for_bearer(&self.config.base_url)?;
        }
        let body = build_generate_body(&req, true);
        self.open_ndjson(self.config.generate_url(), body, req.model)
            .await
    }

    /// Open an NDJSON stream against `url` with `body`, returning the parsed
    /// chunk stream once the upstream status is known to be 2xx. Shared by the
    /// chat and generate streaming entry points; `model` is only used to map a
    /// 404 onto [`ProviderError::ModelNotAvailable`].
    async fn open_ndjson(
        &self,
        url: String,
        body: serde_json::Value,
        model: ModelId,
    ) -> Result<OllamaChunkStream, ProviderError> {
        // No per-request timeout on the stream: the timeout caps a whole request
        // including body read, which would cut off a long generation. Cancellation
        // is the caller dropping the stream instead. The handshake itself (up to
        // the 2xx/non-2xx status) is retried through the shared circuit breaker —
        // no bytes have streamed yet, so a resend is safe.
        let resp = self
            .breaker
            .call(|| {
                retry_with_backoff(
                    &self.config.retry_policy,
                    |a: &AttemptError| a.retryable,
                    || self.ndjson_handshake(&url, &body, &model),
                )
            })
            .await
            .map_err(circuit_error_to_provider_error)?;

        // Map reqwest transport errors to the crate taxonomy up front so the
        // NDJSON parser stays decoupled from `reqwest` (and unit-testable with
        // synthetic chunk streams). `Box::pin` makes the byte stream `Unpin`.
        let bytes = resp.bytes_stream().map(|r| r.map_err(map_stream_error));
        Ok(parse_ndjson(Box::pin(bytes)))
    }

    /// One attempt at the streaming handshake: returns the still-unconsumed
    /// [`reqwest::Response`] on a `2xx` status, or an [`AttemptError`]
    /// classifying whether the failure is worth retrying.
    async fn ndjson_handshake(
        &self,
        url: &str,
        body: &serde_json::Value,
        model: &ModelId,
    ) -> Result<reqwest::Response, AttemptError> {
        let mut request = self.client.post(url).json(body);
        if let Some(key) = &self.config.api_key {
            request = request.bearer_auth(key);
        }

        let resp = request
            .send()
            .await
            .map_err(|e| AttemptError::retryable(map_send_error(e, &self.config.base_url)))?;

        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let retry_after_ms = parse_retry_after_ms(resp.headers());
        let code = status.as_u16();
        let retryable = is_retryable_status(code);
        let body_text = resp.text().await.unwrap_or_default();
        Err(AttemptError::new(
            map_http_error(code, retry_after_ms, &body_text, model),
            retryable,
        ))
    }

    /// One attempt at the non-streaming `complete` call.
    async fn send_completion(
        &self,
        req: &CompletionRequest,
        body: &serde_json::Value,
    ) -> Result<CompletionResponse, AttemptError> {
        let mut request = self
            .client
            .post(self.config.chat_url())
            .timeout(self.config.request_timeout)
            .json(body);
        if let Some(key) = &self.config.api_key {
            request = request.bearer_auth(key);
        }

        let resp = request
            .send()
            .await
            .map_err(|e| AttemptError::retryable(map_send_error(e, &self.config.base_url)))?;

        let status = resp.status();
        if status.is_success() {
            let value: serde_json::Value = resp.json().await.map_err(|e| {
                AttemptError::new(
                    ProviderError::Upstream(format!("decoding response body: {e}")),
                    false,
                )
            })?;
            let parsed: ChatResponse = serde_json::from_value(value.clone()).map_err(|e| {
                AttemptError::new(
                    ProviderError::Upstream(format!("unexpected response shape: {e}")),
                    false,
                )
            })?;
            return Ok(parsed.into_completion(value));
        }

        let retry_after_ms = parse_retry_after_ms(resp.headers());
        let code = status.as_u16();
        let retryable = is_retryable_status(code);
        let body_text = resp.text().await.unwrap_or_default();
        Err(AttemptError::new(
            map_http_error(code, retry_after_ms, &body_text, &req.model),
            retryable,
        ))
    }
}

/// A single failed attempt at an Ollama HTTP call, classifying whether
/// [`retry_with_backoff`] should retry it (a transient network error, `429`,
/// or `5xx`) or stop immediately (`401`/`403`/`400`/`404` — resending an
/// unauthorized or malformed request cannot succeed).
struct AttemptError {
    error: ProviderError,
    retryable: bool,
}

impl AttemptError {
    fn new(error: ProviderError, retryable: bool) -> Self {
        Self { error, retryable }
    }

    fn retryable(error: ProviderError) -> Self {
        Self::new(error, true)
    }
}

/// `429` (rate limited) and any `5xx` (upstream/transient server fault) are
/// worth retrying; anything else (auth, malformed request, unknown model) is
/// permanent and retrying would just repeat the same failure.
fn is_retryable_status(code: u16) -> bool {
    code == 429 || (500..=599).contains(&code)
}

/// Maps the breaker's own `Open` state onto the same [`ProviderError`]
/// taxonomy the underlying HTTP failures use, and unwraps a passed-through
/// inner failure.
fn circuit_error_to_provider_error(err: CircuitError<AttemptError>) -> ProviderError {
    match err {
        CircuitError::Open => ProviderError::Upstream(
            "circuit breaker open: too many recent Ollama failures".to_string(),
        ),
        CircuitError::Inner(attempt) => attempt.error,
    }
}

fn validate_base_url_for_bearer(base_url: &str) -> Result<(), ProviderError> {
    let parsed = reqwest::Url::parse(base_url).map_err(|e| {
        ProviderError::InvalidRequest(format!("Ollama base URL is not a valid URL: {e}"))
    })?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if is_loopback_host(&parsed) => Ok(()),
        "http" => Err(ProviderError::InvalidRequest(
            "Ollama base URL must use https unless it points at localhost/loopback when a bearer token is configured".to_string(),
        )),
        other => Err(ProviderError::InvalidRequest(format!(
            "Ollama base URL must use https or loopback http when a bearer token is configured, got scheme {other:?}"
        ))),
    }
}

fn is_loopback_host(url: &reqwest::Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost") | Some("127.0.0.1") | Some("::1") | Some("[::1]")
    )
}

#[async_trait]
impl Provider for OllamaProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        // The non-streaming path pins `stream: false` — a single buffered reply.
        if self.config.api_key.is_some() {
            validate_base_url_for_bearer(&self.config.base_url)?;
        }
        let body = build_request_body(&req, false);

        self.breaker
            .call(|| {
                retry_with_backoff(
                    &self.config.retry_policy,
                    |a: &AttemptError| a.retryable,
                    || self.send_completion(&req, &body),
                )
            })
            .await
            .map_err(circuit_error_to_provider_error)
    }

    /// Stream a chat completion as shared [`StreamEvent`]s (§3.X) — the uniform
    /// streaming surface that supersedes the crate-local event type.
    ///
    /// Drives [`stream_ndjson`](Self::stream_ndjson) and adapts each
    /// [`OllamaChatChunk`] via [`into_stream_events`]: a non-empty token chunk
    /// becomes a [`StreamEvent::ContentDelta`], and the terminal `done` chunk
    /// becomes a [`StreamEvent::Usage`] (the folded `prompt_eval_count` /
    /// `eval_count`) followed by a [`StreamEvent::Finish`]. The handshake / non-2xx
    /// failure is the `Err` of the returned `Result` (resolved before any event
    /// yields); a mid-stream transport error is an `Err` item. Cancellation is by
    /// drop, exactly as for [`stream_ndjson`](Self::stream_ndjson).
    async fn stream(&self, req: CompletionRequest) -> Result<ProviderStream, ProviderError> {
        let chunks = self.stream_ndjson(req).await?;
        Ok(Box::pin(into_stream_events(chunks)))
    }

    fn id(&self) -> ProviderId {
        ProviderId(PROVIDER_ID.to_string())
    }

    fn supports_streaming(&self) -> bool {
        // §3.4b: the NDJSON streaming path (`stream_ndjson` / `stream_events`)
        // has landed.
        true
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
/// plus `stop` when the request carries stop sequences. `stream` is the caller's
/// choice: [`Provider::complete`] passes `false` (one buffered reply), the
/// §3.4b streaming path passes `true` (NDJSON token chunks).
fn build_request_body(req: &CompletionRequest, stream: bool) -> serde_json::Value {
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

    serde_json::json!({
        "model": req.model.0,
        "messages": messages,
        "stream": stream,
        "options": build_options(req),
    })
}

/// Serialize a [`CompletionRequest`] into Ollama's `/api/generate` request body.
///
/// `/api/generate` is the single-prompt completion endpoint (no chat roles), so
/// the request's messages are flattened into one newline-joined `prompt`. The
/// `options` and `stream` flag carry over from [`build_request_body`].
fn build_generate_body(req: &CompletionRequest, stream: bool) -> serde_json::Value {
    let prompt = req
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    serde_json::json!({
        "model": req.model.0,
        "prompt": prompt,
        "stream": stream,
        "options": build_options(req),
    })
}

/// The shared `options` object: `temperature`, `num_predict` (Ollama's name for
/// the output-token cap), and `stop` when the request carries stop sequences.
fn build_options(req: &CompletionRequest) -> serde_json::Value {
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
    options
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

/// Map a reqwest failure that occurs *mid-stream* (after a 2xx handshake, while
/// pulling NDJSON bytes) onto the crate's error taxonomy. A timeout or dropped
/// connection here is a transport failure, so it surfaces as a
/// [`NetworkFailure`](ProviderError::NetworkFailure).
fn map_stream_error(e: reqwest::Error) -> ProviderError {
    ProviderError::NetworkFailure(e.to_string())
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
            cost_cents: None,
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

// ---------------------------------------------------------------------------
// §3.4b — NDJSON streaming
// ---------------------------------------------------------------------------

/// One newline-delimited JSON object from an Ollama streaming response.
///
/// Ollama emits one of these per generated chunk. A non-terminal chunk carries a
/// partial token (under `message.content` for `/api/chat`, under `response` for
/// `/api/generate`) with `done: false`; the final chunk has `done: true`, an
/// empty token, a `done_reason`, and the run's `prompt_eval_count` /
/// `eval_count`. Every field is optional so either endpoint's shape decodes.
#[derive(Clone, Debug, Default, Deserialize)]
#[non_exhaustive]
pub struct OllamaChatChunk {
    /// The model that produced the chunk (echoed back by Ollama).
    #[serde(default)]
    pub model: Option<String>,
    /// The chat token, for `/api/chat` (`message.content`).
    #[serde(default)]
    pub message: Option<ChunkMessage>,
    /// The completion token, for `/api/generate` (`response`).
    #[serde(default)]
    pub response: Option<String>,
    /// Whether this is the terminal chunk.
    #[serde(default)]
    pub done: bool,
    /// Why generation stopped — set on the terminal chunk (`"stop"`, `"length"`).
    #[serde(default)]
    pub done_reason: Option<String>,
    /// Prompt tokens evaluated → billed as input. Present on the final chunk.
    #[serde(default)]
    pub prompt_eval_count: Option<u32>,
    /// Tokens generated → billed as output. Present on the final chunk.
    #[serde(default)]
    pub eval_count: Option<u32>,
}

/// The assistant message inside a chat [`OllamaChatChunk`].
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ChunkMessage {
    /// The partial token text. Empty/absent on the terminal chunk.
    #[serde(default)]
    pub content: Option<String>,
}

impl OllamaChatChunk {
    /// The token text this chunk carries, from whichever endpoint shape applies
    /// (`message.content` for chat, `response` for generate). `""` when neither
    /// is set (e.g. the terminal chunk).
    #[must_use]
    pub fn token(&self) -> &str {
        if let Some(content) = self.message.as_ref().and_then(|m| m.content.as_deref()) {
            return content;
        }
        self.response.as_deref().unwrap_or("")
    }

    /// Whether this is the terminal (`done: true`) chunk.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// The token-count ledger from this chunk — meaningful on the terminal
    /// chunk, which is the one Ollama stamps the counts onto.
    #[must_use]
    pub fn usage(&self) -> Usage {
        Usage {
            tokens_in: self.prompt_eval_count.unwrap_or(0),
            tokens_out: self.eval_count.unwrap_or(0),
            cost_cents: None,
        }
    }

    /// The [`FinishReason`] for this chunk's `done_reason`.
    #[must_use]
    pub fn finish_reason(&self) -> FinishReason {
        map_finish_reason(self.done_reason.as_deref())
    }
}

/// Adapt a parsed-chunk stream onto the shared [`StreamEvent`] surface (§3.X).
///
/// Each non-terminal chunk with a non-empty token becomes a
/// [`StreamEvent::ContentDelta`]; the terminal `done` chunk fans out into a
/// [`StreamEvent::Usage`] (the run's folded `prompt_eval_count` /
/// `eval_count`) followed by a [`StreamEvent::Finish`] carrying the mapped
/// `done_reason` — the same usage-then-finish order the receipt is minted from.
/// An empty token chunk (e.g. an opener carrying no text) yields nothing, and a
/// mid-stream `Err` is forwarded unchanged.
///
/// Ollama bills no dollar cost, so — unlike the buffered [`complete`] path —
/// there is no cost ledger here: the shared [`StreamEvent::Usage`] carries token
/// counts only, and the consumer prices them through the provider's zeroed
/// [`RateCard`].
fn into_stream_events<S>(chunks: S) -> impl Stream<Item = Result<StreamEvent, ProviderError>> + Send
where
    S: Stream<Item = Result<OllamaChatChunk, ProviderError>> + Send + 'static,
{
    chunks.flat_map(|res| {
        let events: Vec<Result<StreamEvent, ProviderError>> = match res {
            Ok(chunk) if chunk.is_done() => vec![
                Ok(StreamEvent::Usage(chunk.usage())),
                Ok(StreamEvent::Finish(chunk.finish_reason())),
            ],
            Ok(chunk) => {
                let token = chunk.token();
                if token.is_empty() {
                    Vec::new()
                } else {
                    vec![Ok(StreamEvent::ContentDelta(token.to_string()))]
                }
            }
            Err(e) => vec![Err(e)],
        };
        futures::stream::iter(events)
    })
}

/// Parser state for [`parse_ndjson`]: the byte source, a carry buffer for a
/// partial line that spans chunk boundaries, and whether the source is drained.
struct NdjsonState<S> {
    stream: S,
    buf: Vec<u8>,
    finished: bool,
}

/// Turn a stream of raw byte chunks into a stream of parsed [`OllamaChatChunk`]s,
/// one per newline-delimited JSON line.
///
/// Network chunk boundaries do not align with line boundaries, so bytes are
/// buffered until a `\n` completes a line; a trailing line with no newline (e.g.
/// at end-of-stream) is still parsed. A malformed line is an `Err` item; a
/// transport error from the source is forwarded as an `Err` item and ends the
/// stream. Decoupled from `reqwest` (the source yields
/// `Result<_, ProviderError>`) so it is unit-testable with synthetic chunks.
///
/// The result is boxed (a `BoxStream`) so it is `Unpin` — callers can drive it
/// with `StreamExt::next` directly, and dropping it is the cancellation path.
fn parse_ndjson<S, B>(stream: S) -> OllamaChunkStream
where
    S: Stream<Item = Result<B, ProviderError>> + Unpin + Send + 'static,
    B: AsRef<[u8]> + Send + 'static,
{
    let state = NdjsonState {
        stream,
        buf: Vec::new(),
        finished: false,
    };
    futures::stream::unfold(state, |mut state| async move {
        loop {
            // Emit a complete buffered line if one is available.
            if let Some(pos) = state.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = state.buf.drain(..=pos).collect();
                let trimmed = trim_line(&line);
                if trimmed.is_empty() {
                    continue;
                }
                return Some((parse_chunk(trimmed), state));
            }
            // No newline buffered. If the source is drained, flush a trailing
            // partial line (if any) once, then end.
            if state.finished {
                if state.buf.is_empty() {
                    return None;
                }
                let line = std::mem::take(&mut state.buf);
                let trimmed = trim_line(&line);
                if trimmed.is_empty() {
                    return None;
                }
                return Some((parse_chunk(trimmed), state));
            }
            // Pull more bytes.
            match state.stream.next().await {
                Some(Ok(bytes)) => state.buf.extend_from_slice(bytes.as_ref()),
                Some(Err(e)) => {
                    state.finished = true;
                    return Some((Err(e), state));
                }
                None => state.finished = true,
            }
        }
    })
    .boxed()
}

/// Strip a single trailing `\n` and/or `\r` from a buffered line.
fn trim_line(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && (line[end - 1] == b'\n' || line[end - 1] == b'\r') {
        end -= 1;
    }
    &line[..end]
}

/// Deserialize one NDJSON line into an [`OllamaChatChunk`], mapping a JSON error
/// onto [`ProviderError::Upstream`].
fn parse_chunk(bytes: &[u8]) -> Result<OllamaChatChunk, ProviderError> {
    serde_json::from_slice(bytes)
        .map_err(|e| ProviderError::Upstream(format!("malformed NDJSON stream chunk: {e}")))
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

        let body = build_request_body(&req, false);
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
        let body = build_request_body(&req, false);
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
    fn provider_id_is_ollama_and_streaming() {
        let provider = OllamaProvider::new(OllamaConfig::new());
        assert_eq!(provider.id(), ProviderId("ollama".to_string()));
        // §3.4b: the NDJSON streaming path is live.
        assert!(provider.supports_streaming());
        assert_eq!(provider.model_id().0, DEFAULT_MODEL);
    }

    #[test]
    fn generate_url_appends_endpoint() {
        let cfg = OllamaConfig::new().base_url("http://localhost:11434/");
        assert_eq!(cfg.generate_url(), "http://localhost:11434/api/generate");
    }

    #[test]
    fn stream_request_body_sets_stream_true() {
        use ardur_runtime::ChatMessage;
        let req =
            CompletionRequest::new(vec![ChatMessage::user("hi")], ModelId::new("llama3.2"), 32);
        let body = build_request_body(&req, true);
        assert_eq!(body["stream"], true);
        assert_eq!(body["options"]["num_predict"], 32);
    }

    #[test]
    fn generate_body_flattens_messages_to_prompt() {
        use ardur_runtime::ChatMessage;
        let req = CompletionRequest::new(
            vec![ChatMessage::system("be terse"), ChatMessage::user("hi")],
            ModelId::new("llama3.2"),
            16,
        );
        let body = build_generate_body(&req, true);
        assert_eq!(body["prompt"], "be terse\nhi");
        assert_eq!(body["stream"], true);
        assert!(body.get("messages").is_none(), "generate has no messages");
    }

    #[test]
    fn chunk_token_reads_chat_then_generate_shape() {
        // Chat shape: token under message.content.
        let chat: OllamaChatChunk = serde_json::from_value(serde_json::json!({
            "message": {"content": "he"},
            "done": false
        }))
        .unwrap();
        assert_eq!(chat.token(), "he");
        assert!(!chat.is_done());

        // Generate shape: token under response.
        let generated: OllamaChatChunk = serde_json::from_value(serde_json::json!({
            "response": "llo",
            "done": false
        }))
        .unwrap();
        assert_eq!(generated.token(), "llo");

        // Terminal chunk: no token, counts present.
        let done: OllamaChatChunk = serde_json::from_value(serde_json::json!({
            "done": true,
            "done_reason": "stop",
            "prompt_eval_count": 9,
            "eval_count": 4
        }))
        .unwrap();
        assert!(done.is_done());
        assert_eq!(done.token(), "");
        assert_eq!(
            done.usage(),
            Usage {
                tokens_in: 9,
                tokens_out: 4,
                cost_cents: None,
            }
        );
        assert!(matches!(done.finish_reason(), FinishReason::Stop));
    }

    #[tokio::test]
    async fn into_stream_events_maps_chunks_to_shared_events() {
        // A token chunk → ContentDelta; the done chunk → Usage then Finish.
        let token: OllamaChatChunk = serde_json::from_value(serde_json::json!({
            "message": {"content": "hi"},
            "done": false
        }))
        .unwrap();
        let done: OllamaChatChunk = serde_json::from_value(serde_json::json!({
            "done": true,
            "done_reason": "stop",
            "prompt_eval_count": 9,
            "eval_count": 4
        }))
        .unwrap();
        let chunks = futures::stream::iter(vec![Ok(token), Ok(done)]);
        let events: Vec<StreamEvent> = into_stream_events(chunks)
            .map(|r| r.expect("each event is Ok"))
            .collect()
            .await;
        assert_eq!(
            events,
            vec![
                StreamEvent::ContentDelta("hi".to_string()),
                StreamEvent::Usage(Usage {
                    tokens_in: 9,
                    tokens_out: 4,
                    cost_cents: None,
                }),
                StreamEvent::Finish(FinishReason::Stop),
            ]
        );
    }
}
