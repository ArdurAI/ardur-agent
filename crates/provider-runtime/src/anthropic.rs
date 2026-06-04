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
// TODO §3.0 Phase 2: token streaming (server-sent events) and multi-turn cost
// projection at admission. (§6.0 added `tool_use` request/response wiring.)

use std::collections::BTreeMap;
use std::collections::VecDeque;

use ardur_runtime::{ChatMessage, CostTuple, ProviderId, Role, ToolCall};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::Deserialize;

use crate::error::ProviderError;
use crate::provider::Provider;
use crate::rate_card::RateCard;
use crate::stream::{ProviderStream, StreamEvent, events_from_response, iter_events};
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
    /// The Messages-API endpoint requests are posted to. Defaults to
    /// [`MESSAGES_URL`]; [`with_base_url`](Self::with_base_url) repoints it at a
    /// mock server so the live HTTP/SSE path is exercised offline.
    base_url: String,
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
            base_url: MESSAGES_URL.to_string(),
        }
    }

    /// Repoint the provider at `base_url` instead of the public Messages-API
    /// endpoint (builder-style). Used by the streaming/round-trip tests to drive
    /// the live HTTP and SSE paths against a local `wiremock` server.
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
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
            base_url: MESSAGES_URL.to_string(),
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
            .post(&self.base_url)
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

    /// Open the live Messages-API call in streaming mode and return a
    /// [`ProviderStream`] of decoded [`StreamEvent`]s (§3.1b).
    ///
    /// The request body is the same one [`complete`](Provider::complete) sends,
    /// with `"stream": true` added. A non-2xx status is mapped to a
    /// [`ProviderError`] *before* any stream is returned (the `Err` arm), so the
    /// caller sees admission failures synchronously. On success the response's
    /// raw byte stream is parsed into SSE frames by `eventsource-stream` and
    /// folded — incrementally, one frame at a time — into the event sequence,
    /// preserving cancellation: dropping the returned stream drops the in-flight
    /// `reqwest` response and aborts the upstream request.
    async fn stream_live(
        &self,
        client: &reqwest::Client,
        req: CompletionRequest,
    ) -> Result<ProviderStream, ProviderError> {
        if self.api_key.is_empty() {
            return Err(ProviderError::Unauthorized);
        }

        let mut body = build_request_body(&req);
        body.as_object_mut()
            .expect("build_request_body always returns a JSON object")
            .insert("stream".to_string(), serde_json::Value::Bool(true));

        let resp = client
            .post(&self.base_url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::NetworkFailure(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let retry_after_ms = parse_retry_after_ms(resp.headers());
            let code = status.as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(map_http_error(code, retry_after_ms, &body, &req.model));
        }

        // `bytes_stream()` is the raw chunk source; `.eventsource()` reframes it
        // into `event: …\ndata: …\n\n` SSE messages. Box it so the concrete
        // (unnameable) stream type can live behind the `unfold` state.
        let events = resp.bytes_stream().eventsource();
        let state = SseState::new(Box::pin(events));

        // `unfold` pulls one SSE frame at a time, turning each into zero-or-more
        // queued `StreamEvent`s and emitting them one per poll — so the stream
        // stays lazy and cancel-safe rather than buffering the whole response.
        let stream = futures::stream::unfold(state, |mut state| async move {
            loop {
                if let Some(item) = state.queue.pop_front() {
                    return Some((item, state));
                }
                if state.terminated {
                    return None;
                }
                match state.events.next().await {
                    // Underlying stream exhausted: a well-formed response already
                    // queued its `message_stop` events, so nothing remains.
                    None => return None,
                    Some(Ok(event)) => state.handle(&event),
                    Some(Err(e)) => {
                        state.terminated = true;
                        return Some((
                            Err(ProviderError::Upstream(format!("SSE stream error: {e}"))),
                            state,
                        ));
                    }
                }
            }
        });

        Ok(Box::pin(stream))
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

    /// Stream the completion as decoded [`StreamEvent`]s (§3.1b). The live
    /// backend parses real Messages-API server-sent events; the stub replays its
    /// fixed response through the shared [`events_from_response`] flattening, so
    /// the stub streams without a network call exactly as the default impl would.
    async fn stream(&self, req: CompletionRequest) -> Result<ProviderStream, ProviderError> {
        match &self.backend {
            Backend::Stub => Ok(iter_events(events_from_response(Self::stub_response()))),
            Backend::Live(client) => self.stream_live(client, req).await,
        }
    }

    fn id(&self) -> ProviderId {
        ProviderId("anthropic".to_string())
    }

    fn supports_streaming(&self) -> bool {
        // §3.1b: the SSE streaming path (`Provider::stream`) is live.
        true
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// Serialize a [`CompletionRequest`] into the Messages-API request body.
///
/// System messages flatten into the top-level `system` field (joined with blank
/// lines when there are several); user/assistant turns become the `messages`
/// array. An assistant turn that requested tools serializes its calls as
/// `tool_use` content blocks (§6.0), and a run of [`Role::Tool`] results becomes
/// one user turn of `tool_result` blocks (the Messages-API shape, which requires
/// tool results to ride in a user message). `system`, `stop_sequences`, and
/// `tools` are omitted when empty — so a no-tool request is byte-identical to
/// the pre-§6.0 body.
fn build_request_body(req: &CompletionRequest) -> serde_json::Value {
    let mut messages: Vec<serde_json::Value> = Vec::with_capacity(req.messages.len());
    let mut system_parts = Vec::new();
    let msgs = &req.messages;
    let mut i = 0;
    while i < msgs.len() {
        let m = &msgs[i];
        match m.role {
            Role::System => {
                system_parts.push(m.content.clone());
                i += 1;
            }
            Role::User => {
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": m.content,
                }));
                i += 1;
            }
            Role::Assistant => {
                messages.push(assistant_message(m));
                i += 1;
            }
            Role::Tool => {
                // Coalesce the run of consecutive tool results into one user turn
                // of `tool_result` blocks (each keyed to the `tool_use` id it
                // answers), so a multi-tool round trips in a single user message.
                let mut blocks: Vec<serde_json::Value> = Vec::new();
                while i < msgs.len() && matches!(msgs[i].role, Role::Tool) {
                    let t = &msgs[i];
                    let mut block = serde_json::json!({
                        "type": "tool_result",
                        "content": t.content,
                    });
                    if let Some(id) = &t.tool_call_id {
                        block
                            .as_object_mut()
                            .expect("json! object literal is always a map")
                            .insert(
                                "tool_use_id".to_string(),
                                serde_json::Value::String(id.clone()),
                            );
                    }
                    blocks.push(block);
                    i += 1;
                }
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": blocks,
                }));
            }
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
    if !req.tools.is_empty() {
        let tools: Vec<serde_json::Value> = req
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();
        map.insert("tools".to_string(), serde_json::Value::Array(tools));
    }
    body
}

/// Serialize an assistant turn. A plain turn is `{role, content: <text>}`; a turn
/// that requested tools becomes a content-block array of the optional leading
/// `text` block followed by one `tool_use` block per [`ToolCall`].
fn assistant_message(m: &ChatMessage) -> serde_json::Value {
    if m.tool_calls.is_empty() {
        return serde_json::json!({
            "role": "assistant",
            "content": m.content,
        });
    }
    let mut blocks: Vec<serde_json::Value> = Vec::new();
    if !m.content.is_empty() {
        blocks.push(serde_json::json!({ "type": "text", "text": m.content }));
    }
    for call in &m.tool_calls {
        blocks.push(serde_json::json!({
            "type": "tool_use",
            "id": call.id,
            "name": call.name,
            "input": call.arguments,
        }));
    }
    serde_json::json!({
        "role": "assistant",
        "content": blocks,
    })
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

/// One block of the response `content` array. `text` blocks fold into the
/// completion content; `tool_use` blocks (carrying `id`, `name`, and the JSON
/// `input`) decode into [`ToolCall`]s the runtime dispatches (§6.0).
#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
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

        // Decode any `tool_use` blocks into the calls the runtime will dispatch.
        let tool_calls: Vec<ToolCall> = self
            .content
            .iter()
            .filter(|b| b.block_type == "tool_use")
            .filter_map(|b| match (&b.id, &b.name) {
                (Some(id), Some(name)) => Some(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: b.input.clone().unwrap_or(serde_json::Value::Null),
                }),
                _ => None,
            })
            .collect();

        let finish_reason = match self.stop_reason.as_deref() {
            Some("end_turn") | None => FinishReason::Stop,
            Some("max_tokens") => FinishReason::MaxTokens,
            Some("stop_sequence") => {
                FinishReason::StopSequence(self.stop_sequence.unwrap_or_default())
            }
            Some("tool_use") => FinishReason::ToolUse(tool_calls),
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

/// The boxed SSE-frame stream the [`SseState`] folds events out of — the decoded
/// `event:`/`data:` messages from `eventsource-stream`, errored by the
/// underlying `reqwest` transport.
type SseEvents = std::pin::Pin<
    Box<
        dyn futures::Stream<
                Item = Result<
                    eventsource_stream::Event,
                    eventsource_stream::EventStreamError<reqwest::Error>,
                >,
            > + Send,
    >,
>;

/// A tool call being assembled across streamed frames: its id/name are known
/// from the `content_block_start`, and its JSON `arguments` accrete from the
/// `input_json_delta` fragments.
struct ToolAccum {
    id: String,
    name: String,
    json: String,
}

/// The fold state the Anthropic SSE parser threads through `unfold`.
///
/// It owns the decoded-frame stream, a `queue` of events ready to emit (one
/// frame can produce several), the running token `ledger`, the per-index
/// `tool_blocks` being assembled, and the `stop_reason`/`stop_sequence` captured
/// from `message_delta` so `message_stop` can mint the terminal
/// [`FinishReason`].
struct SseState {
    events: SseEvents,
    queue: VecDeque<Result<StreamEvent, ProviderError>>,
    ledger: Usage,
    tool_blocks: BTreeMap<u64, ToolAccum>,
    stop_reason: Option<String>,
    stop_sequence: Option<String>,
    terminated: bool,
}

impl SseState {
    /// Seed the parser over a decoded SSE-frame stream.
    fn new(events: SseEvents) -> Self {
        Self {
            events,
            queue: VecDeque::new(),
            ledger: Usage::default(),
            tool_blocks: BTreeMap::new(),
            stop_reason: None,
            stop_sequence: None,
            terminated: false,
        }
    }

    /// Fold one decoded SSE frame into zero-or-more queued [`StreamEvent`]s.
    ///
    /// Mirrors the Messages-API streaming protocol: `message_start` seeds input
    /// usage, `content_block_start` opens a text or `tool_use` block,
    /// `content_block_delta` carries text or tool-argument fragments,
    /// `message_delta` records the stop reason and final output usage, and
    /// `message_stop` emits the final usage and terminal finish. A malformed or
    /// unrecognized frame is skipped; an `error` frame is surfaced as an
    /// `Upstream` error item.
    fn handle(&mut self, event: &eventsource_stream::Event) {
        // Frames carry a JSON `data:` payload; skip any that fails to parse
        // rather than aborting the whole stream on one bad frame.
        let data: serde_json::Value = match serde_json::from_str(&event.data) {
            Ok(v) => v,
            Err(_) => return,
        };

        match event.event.as_str() {
            "message_start" => {
                let usage = &data["message"]["usage"];
                self.ledger.tokens_in = usage["input_tokens"].as_u64().unwrap_or(0) as u32;
                self.ledger.tokens_out = usage["output_tokens"].as_u64().unwrap_or(0) as u32;
                self.queue.push_back(Ok(StreamEvent::Usage(self.ledger)));
            }
            "content_block_start" => {
                let index = data["index"].as_u64().unwrap_or(0);
                let block = &data["content_block"];
                if block["type"].as_str() == Some("tool_use") {
                    let id = block["id"].as_str().unwrap_or_default().to_string();
                    let name = block["name"].as_str().unwrap_or_default().to_string();
                    self.tool_blocks.insert(
                        index,
                        ToolAccum {
                            id: id.clone(),
                            name: name.clone(),
                            json: String::new(),
                        },
                    );
                    self.queue
                        .push_back(Ok(StreamEvent::ToolCallStart(ToolCall {
                            id,
                            name,
                            arguments: serde_json::Value::Null,
                        })));
                }
            }
            "content_block_delta" => {
                let index = data["index"].as_u64().unwrap_or(0);
                let delta = &data["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        if let Some(text) = delta["text"].as_str() {
                            self.queue
                                .push_back(Ok(StreamEvent::ContentDelta(text.to_string())));
                        }
                    }
                    Some("input_json_delta") => {
                        let fragment = delta["partial_json"].as_str().unwrap_or_default();
                        if let Some(accum) = self.tool_blocks.get_mut(&index) {
                            accum.json.push_str(fragment);
                            self.queue.push_back(Ok(StreamEvent::ToolCallDelta {
                                id: accum.id.clone(),
                                delta: fragment.to_string(),
                            }));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {}
            "message_delta" => {
                if let Some(reason) = data["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(reason.to_string());
                }
                if let Some(seq) = data["delta"]["stop_sequence"].as_str() {
                    self.stop_sequence = Some(seq.to_string());
                }
                if let Some(out) = data["usage"]["output_tokens"].as_u64() {
                    self.ledger.tokens_out = out as u32;
                }
                self.queue.push_back(Ok(StreamEvent::Usage(self.ledger)));
            }
            "message_stop" => {
                // Final ledger then terminal finish — the order a consumer mints
                // the receipt from, matching the non-streaming cost-gate flow.
                self.queue.push_back(Ok(StreamEvent::Usage(self.ledger)));
                self.queue
                    .push_back(Ok(StreamEvent::Finish(self.assemble_finish_reason())));
            }
            "error" => {
                let message = data["error"]["message"]
                    .as_str()
                    .unwrap_or("anthropic streaming error")
                    .to_string();
                self.queue.push_back(Err(ProviderError::Upstream(message)));
            }
            // `ping` and any future event types are intentionally ignored.
            _ => {}
        }
    }

    /// Build the terminal [`FinishReason`] from the captured stop reason,
    /// assembling any `tool_use` blocks into complete [`ToolCall`]s (their JSON
    /// arguments parsed from the accreted `input_json_delta` fragments). Mirrors
    /// the non-streaming [`MessagesResponse`] mapping.
    fn assemble_finish_reason(&self) -> FinishReason {
        match self.stop_reason.as_deref() {
            Some("end_turn") | None => FinishReason::Stop,
            Some("max_tokens") => FinishReason::MaxTokens,
            Some("stop_sequence") => {
                FinishReason::StopSequence(self.stop_sequence.clone().unwrap_or_default())
            }
            Some("tool_use") => {
                let calls = self
                    .tool_blocks
                    .values()
                    .map(|accum| ToolCall {
                        id: accum.id.clone(),
                        name: accum.name.clone(),
                        arguments: serde_json::from_str(&accum.json)
                            .unwrap_or(serde_json::Value::Null),
                    })
                    .collect();
                FinishReason::ToolUse(calls)
            }
            Some(other) => FinishReason::Error(format!("unknown stop_reason: {other}")),
        }
    }
}
