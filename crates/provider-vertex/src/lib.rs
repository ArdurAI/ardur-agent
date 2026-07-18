//! ardur-provider-vertex — Google Vertex AI (Gemini) backend (§3.4).
//!
//! Targets Vertex's `generateContent` REST endpoint:
//! `https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:generateContent`.
//!
//! Standard Vertex auth is a Google service-account OAuth2 flow (an
//! RS256-signed JWT assertion exchanged for a bearer token). The workspace
//! has no RSA crate (`p256` is ECDSA/P-256 only), and adding one is out of
//! scope for this pass — pulling in a new asymmetric-crypto dependency
//! deserves its own review, not a rider on a provider crate. Phase 1 instead
//! accepts an already-minted bearer token via [`ACCESS_TOKEN_ENV`] — the
//! caller mints it however they already do (`gcloud auth print-access-token`,
//! a sidecar, workload identity) and this crate just uses it.
//!
//! # Phase 1 (this crate)
//!
//! - [`VertexProvider`] — [`Provider::complete`] against a real
//!   `generateContent` call, with tool-calling mapped onto Gemini's
//!   `functionDeclarations`/`functionCall`/`functionResponse` shape.
//! - `supports_streaming()` is `true`: [`Provider::stream`] calls
//!   `streamGenerateContent?alt=sse` and decodes Gemini's SSE feed into the
//!   shared [`StreamEvent`](ardur_provider_runtime::StreamEvent) protocol —
//!   see [`streaming`].
//!
//! Phase 2 TODO (not this crate): local service-account JWT minting (needs an
//! RSA crate — separate review); `textembedding-gecko` embeddings.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::HashMap;
use std::fmt;

use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, ModelId, Provider, ProviderError,
    ProviderStream, RateCard, ToolCall, Usage,
};
use ardur_runtime::{ChatMessage, CostTuple, ProviderId, Role};
use async_trait::async_trait;
use serde::Deserialize;

mod streaming;

/// The registry key this backend answers to.
const PROVIDER_ID: &str = "vertex";
/// `ARDUR_VERTEX_ACCESS_TOKEN` — a pre-minted OAuth2 bearer token.
pub const ACCESS_TOKEN_ENV: &str = "ARDUR_VERTEX_ACCESS_TOKEN";
/// `ARDUR_VERTEX_PROJECT` — the GCP project id.
pub const PROJECT_ENV: &str = "ARDUR_VERTEX_PROJECT";
/// `ARDUR_VERTEX_LOCATION` — optional, defaults to [`DEFAULT_LOCATION`].
pub const LOCATION_ENV: &str = "ARDUR_VERTEX_LOCATION";
/// `ARDUR_VERTEX_MODEL` — optional, defaults to [`DEFAULT_MODEL`].
pub const MODEL_ENV: &str = "ARDUR_VERTEX_MODEL";
/// The default Vertex region when [`LOCATION_ENV`] is unset.
pub const DEFAULT_LOCATION: &str = "us-central1";
/// The default Gemini model when [`MODEL_ENV`] is unset.
pub const DEFAULT_MODEL: &str = "gemini-1.5-pro";

/// How a [`VertexProvider`] connects to its Vertex project.
#[derive(Clone)]
pub struct VertexConfig {
    access_token: String,
    project: String,
    location: String,
    base_url_override: Option<String>,
}

impl fmt::Debug for VertexConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VertexConfig")
            .field("access_token", &redacted_present(&self.access_token))
            .field("project", &self.project)
            .field("location", &self.location)
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

impl VertexConfig {
    /// A config bound to a bearer token and project, with the default
    /// location.
    pub fn new(access_token: impl Into<String>, project: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            project: project.into(),
            location: DEFAULT_LOCATION.to_string(),
            base_url_override: None,
        }
    }

    /// Read config from environment: [`ACCESS_TOKEN_ENV`] and
    /// [`PROJECT_ENV`] are required; [`LOCATION_ENV`] is optional.
    ///
    /// Returns [`ProviderError::Unauthorized`] when the token is unset, and
    /// [`ProviderError::InvalidRequest`] when the project is unset.
    pub fn from_env() -> Result<Self, ProviderError> {
        let token = nonempty_env(ACCESS_TOKEN_ENV).ok_or(ProviderError::Unauthorized)?;
        let project = nonempty_env(PROJECT_ENV)
            .ok_or_else(|| ProviderError::InvalidRequest(format!("{PROJECT_ENV} must be set")))?;

        let mut config = Self::new(token, project);
        if let Some(location) = nonempty_env(LOCATION_ENV) {
            config.location = location;
        }
        Ok(config)
    }

    /// Override the scheme+host, for pointing at a mock server in tests.
    #[must_use]
    pub fn base_url_override(mut self, base_url: impl Into<String>) -> Self {
        self.base_url_override = Some(base_url.into());
        self
    }

    fn base(&self) -> String {
        self.base_url_override
            .clone()
            .unwrap_or_else(|| format!("https://{}-aiplatform.googleapis.com", self.location))
    }

    fn generate_content_url(&self, model: &str) -> String {
        format!(
            "{}/v1/projects/{}/locations/{}/publishers/google/models/{}:generateContent",
            self.base(),
            self.project,
            self.location,
            model
        )
    }

    /// The `streamGenerateContent` URL for `model`, with `alt=sse` so Vertex
    /// answers with a `text/event-stream` rather than a raw JSON-array
    /// chunked response.
    fn stream_generate_content_url(&self, model: &str) -> String {
        format!(
            "{}/v1/projects/{}/locations/{}/publishers/google/models/{}:streamGenerateContent?alt=sse",
            self.base(),
            self.project,
            self.location,
            model
        )
    }
}

fn nonempty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The Google Vertex AI (Gemini) provider.
pub struct VertexProvider {
    config: VertexConfig,
    model_id: ModelId,
    rate_card: RateCard,
    client: reqwest::Client,
}

impl VertexProvider {
    /// Build a live provider from `config`, defaulting completions to
    /// `model_id`.
    #[must_use]
    pub fn new(config: VertexConfig, model_id: ModelId) -> Self {
        Self {
            config,
            model_id,
            rate_card: vertex_passthrough_rate_card(),
            client: reqwest::Client::new(),
        }
    }

    /// Build a live provider reading [`VertexConfig::from_env`], defaulting
    /// completions to [`MODEL_ENV`] (or [`DEFAULT_MODEL`] when unset).
    pub fn from_env() -> Result<Self, ProviderError> {
        let model_id =
            ModelId::new(nonempty_env(MODEL_ENV).unwrap_or_else(|| DEFAULT_MODEL.to_string()));
        Ok(Self::new(VertexConfig::from_env()?, model_id))
    }

    /// The model this provider defaults completions to.
    #[must_use]
    pub fn model_id(&self) -> &ModelId {
        &self.model_id
    }
}

#[async_trait]
impl Provider for VertexProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        if self.config.access_token.is_empty() {
            return Err(ProviderError::Unauthorized);
        }

        let body = build_request_body(&req);
        let resp = self
            .client
            .post(self.config.generate_content_url(&req.model.0))
            .bearer_auth(&self.config.access_token)
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
            let parsed: GenerateContentResponse = serde_json::from_value(value.clone())
                .map_err(|e| ProviderError::Upstream(format!("unexpected response shape: {e}")))?;
            return Ok(parsed.into_completion(value));
        }

        let code = status.as_u16();
        let body = resp.text().await.unwrap_or_default();
        Err(map_http_error(code, &body, &req.model))
    }

    /// Stream the completion as shared [`StreamEvent`](ardur_provider_runtime::StreamEvent)s
    /// via [`streaming::into_provider_events`], calling `streamGenerateContent?alt=sse`.
    /// A connect failure or non-2xx status is the `Err` of the returned
    /// `Result` (resolved before any event yields); a mid-stream transport
    /// error is an `Err` item. Cancellation is by drop.
    async fn stream(&self, req: CompletionRequest) -> Result<ProviderStream, ProviderError> {
        if self.config.access_token.is_empty() {
            return Err(ProviderError::Unauthorized);
        }

        let body = build_request_body(&req);
        let resp = self
            .client
            .post(self.config.stream_generate_content_url(&req.model.0))
            .bearer_auth(&self.config.access_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::NetworkFailure(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let code = status.as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(map_http_error(code, &body, &req.model));
        }

        Ok(Box::pin(streaming::into_provider_events(resp)))
    }

    fn id(&self) -> ProviderId {
        ProviderId(PROVIDER_ID.to_string())
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// A zeroed "passthrough" rate card — Vertex pricing is region/model tiered
/// and not one public catalog at this layer; the card exists only to satisfy
/// [`Provider::rate_card`].
fn vertex_passthrough_rate_card() -> RateCard {
    RateCard {
        version_id: "vertex-passthrough-v1".to_string(),
        cents_per_1k_input: 0.0,
        cents_per_1k_output: 0.0,
        cents_per_request: 0.0,
    }
}

/// Serialize a [`CompletionRequest`] into Gemini's `generateContent` body.
///
/// System turns flatten into `systemInstruction`; user/assistant turns become
/// `contents` entries with `role` `"user"`/`"model"` (Gemini's name for the
/// assistant role) and `parts`. An assistant turn that requested tools
/// carries `functionCall` parts instead of `text`. A run of [`Role::Tool`]
/// results becomes one `"function"`-role turn of `functionResponse` parts,
/// each naming the function it answers — looked up from the `tool_call_id` by
/// scanning the transcript's earlier `functionCall`s, since Gemini's
/// `functionResponse` is keyed by name, not by a per-call id the way
/// Anthropic/OpenAI's tool results are.
fn build_request_body(req: &CompletionRequest) -> serde_json::Value {
    let call_names = index_tool_call_names(&req.messages);

    let mut contents: Vec<serde_json::Value> = Vec::with_capacity(req.messages.len());
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
                contents.push(serde_json::json!({
                    "role": "user",
                    "parts": [{"text": m.content}],
                }));
                i += 1;
            }
            Role::Assistant => {
                contents.push(assistant_content(m));
                i += 1;
            }
            Role::Tool => {
                let mut parts: Vec<serde_json::Value> = Vec::new();
                while i < msgs.len() && matches!(msgs[i].role, Role::Tool) {
                    let t = &msgs[i];
                    let name = t
                        .tool_call_id
                        .as_deref()
                        .and_then(|id| call_names.get(id))
                        .cloned()
                        .unwrap_or_default();
                    parts.push(serde_json::json!({
                        "functionResponse": {
                            "name": name,
                            "response": {"content": t.content},
                        },
                    }));
                    i += 1;
                }
                contents.push(serde_json::json!({
                    "role": "function",
                    "parts": parts,
                }));
            }
        }
    }

    let mut body = serde_json::json!({
        "contents": contents,
        "generationConfig": {
            "maxOutputTokens": req.max_tokens,
            "temperature": req.temperature,
        },
    });
    let map = body
        .as_object_mut()
        .expect("json! object literal is always a map");
    if !system_parts.is_empty() {
        map.insert(
            "systemInstruction".to_string(),
            serde_json::json!({ "parts": [{"text": system_parts.join("\n\n")}] }),
        );
    }
    if !req.stop_sequences.is_empty() {
        map["generationConfig"]["stopSequences"] = serde_json::json!(req.stop_sequences);
    }
    if !req.tools.is_empty() {
        let declarations: Vec<serde_json::Value> = req
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                })
            })
            .collect();
        map.insert(
            "tools".to_string(),
            serde_json::json!([{ "functionDeclarations": declarations }]),
        );
    }
    body
}

/// Map each tool-call id in the transcript's assistant turns to the function
/// name it invoked, so a later `functionResponse` can name the function it
/// answers.
fn index_tool_call_names(messages: &[ChatMessage]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for m in messages {
        for call in &m.tool_calls {
            names.insert(call.id.clone(), call.name.clone());
        }
    }
    names
}

fn assistant_content(m: &ChatMessage) -> serde_json::Value {
    if m.tool_calls.is_empty() {
        return serde_json::json!({
            "role": "model",
            "parts": [{"text": m.content}],
        });
    }
    let mut parts: Vec<serde_json::Value> = Vec::new();
    if !m.content.is_empty() {
        parts.push(serde_json::json!({ "text": m.content }));
    }
    for call in &m.tool_calls {
        parts.push(serde_json::json!({
            "functionCall": {
                "name": call.name,
                "args": call.arguments,
            },
        }));
    }
    serde_json::json!({
        "role": "model",
        "parts": parts,
    })
}

/// Map a non-2xx Vertex response onto the crate's [`ProviderError`]
/// taxonomy. Vertex's error envelope matches Google's common API error shape:
/// `{ "error": { "code", "message", "status" } }`.
fn map_http_error(code: u16, body: &str, model: &ModelId) -> ProviderError {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.to_string());
    match code {
        401 | 403 => ProviderError::Unauthorized,
        429 => ProviderError::RateLimited { retry_after_ms: 0 },
        400 => ProviderError::InvalidRequest(message),
        404 => ProviderError::ModelNotAvailable(model.clone()),
        _ => ProviderError::Upstream(format!("HTTP {code}: {message}")),
    }
}

/// The subset of Gemini's `generateContent` response this crate reads.
#[derive(Deserialize)]
struct GenerateContentResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
    #[serde(default)]
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<ApiUsage>,
}

#[derive(Deserialize)]
struct Candidate {
    #[serde(default)]
    content: Option<CandidateContent>,
    #[serde(default)]
    #[serde(rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct CandidateContent {
    #[serde(default)]
    parts: Vec<CandidatePart>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CandidatePart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    function_call: Option<FunctionCallPart>,
}

#[derive(Deserialize)]
struct FunctionCallPart {
    name: String,
    #[serde(default)]
    args: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiUsage {
    #[serde(default)]
    prompt_token_count: u32,
    #[serde(default)]
    candidates_token_count: u32,
}

impl GenerateContentResponse {
    fn into_completion(self, raw: serde_json::Value) -> CompletionResponse {
        let candidate = self.candidates.into_iter().next();
        let (content, tool_calls, finish_raw) = match candidate {
            Some(c) => {
                let parts = c.content.unwrap_or_default().parts;
                let mut text = String::new();
                let mut calls = Vec::new();
                for (idx, part) in parts.into_iter().enumerate() {
                    if let Some(t) = part.text {
                        text.push_str(&t);
                    }
                    if let Some(fc) = part.function_call {
                        // Gemini function calls carry no id; synthesize one
                        // from the part's position so downstream tool-result
                        // matching stays stable within one response.
                        calls.push(ToolCall {
                            id: format!("call_{idx}"),
                            name: fc.name,
                            arguments: fc.args,
                        });
                    }
                }
                (text, calls, c.finish_reason)
            }
            None => (String::new(), Vec::new(), None),
        };
        let finish_reason = map_finish_reason(finish_raw.as_deref(), tool_calls);

        let usage = match self.usage_metadata {
            Some(u) => Usage {
                tokens_in: u.prompt_token_count,
                tokens_out: u.candidates_token_count,
                cost_cents: None,
            },
            None => Usage::default(),
        };

        let cost = CostTuple {
            tokens_in: u64::from(usage.tokens_in),
            tokens_out: u64::from(usage.tokens_out),
            cents: 0,
            wall_ms: 0,
            attention_score: 0,
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

fn map_finish_reason(reason: Option<&str>, tool_calls: Vec<ToolCall>) -> FinishReason {
    if !tool_calls.is_empty() {
        return FinishReason::ToolUse(tool_calls);
    }
    match reason {
        Some("STOP") | None => FinishReason::Stop,
        Some("MAX_TOKENS") => FinishReason::MaxTokens,
        Some("SAFETY") | Some("RECITATION") => {
            FinishReason::Error(format!("generation halted: {}", reason.unwrap_or_default()))
        }
        Some(other) => FinishReason::Error(format!("unknown finishReason: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> VertexConfig {
        VertexConfig::new("token", "my-project")
    }

    #[test]
    fn generate_content_url_is_project_and_location_scoped() {
        let cfg = config();
        assert_eq!(
            cfg.generate_content_url("gemini-1.5-pro"),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/publishers/google/models/gemini-1.5-pro:generateContent"
        );
    }

    #[test]
    fn request_body_maps_roles_to_gemini_shape() {
        let req = CompletionRequest::new(
            vec![
                ChatMessage::system("be terse"),
                ChatMessage::user("hi"),
                ChatMessage::assistant("hello"),
            ],
            ModelId::new("gemini-1.5-pro"),
            64,
        );
        let body = build_request_body(&req);
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be terse");
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["contents"][0]["parts"][0]["text"], "hi");
        assert_eq!(body["contents"][1]["role"], "model");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 64);
    }

    #[test]
    fn tool_result_names_the_function_it_answers() {
        let calls = vec![ToolCall {
            id: "call_1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({"msg": "hi"}),
        }];
        let req = CompletionRequest::new(
            vec![
                ChatMessage::user("call echo"),
                ChatMessage::assistant_tool_calls("", calls),
                ChatMessage::tool_result("call_1", "hi"),
            ],
            ModelId::new("gemini-1.5-pro"),
            64,
        );
        let body = build_request_body(&req);
        let function_turn = &body["contents"][2];
        assert_eq!(function_turn["role"], "function");
        assert_eq!(
            function_turn["parts"][0]["functionResponse"]["name"],
            "echo"
        );
    }

    #[test]
    fn debug_redacts_access_token() {
        let cfg = VertexConfig::new("DUMMY_TOKEN_FOR_TESTING_ONLY", "p");
        let rendered = format!("{cfg:?}");
        assert!(
            !rendered.contains("DUMMY_TOKEN_FOR_TESTING_ONLY"),
            "{rendered}"
        );
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn provider_id_is_vertex() {
        let provider = VertexProvider::new(config(), ModelId::new("gemini-1.5-pro"));
        assert_eq!(provider.id(), ProviderId("vertex".to_string()));
        assert!(provider.supports_streaming());
    }

    #[test]
    fn stream_generate_content_url_requests_sse() {
        let cfg = config();
        assert_eq!(
            cfg.stream_generate_content_url("gemini-1.5-pro"),
            "https://us-central1-aiplatform.googleapis.com/v1/projects/my-project/locations/us-central1/publishers/google/models/gemini-1.5-pro:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn empty_token_is_unauthorized() {
        let provider =
            VertexProvider::new(VertexConfig::new("", "p"), ModelId::new("gemini-1.5-pro"));
        let req = CompletionRequest::new(
            vec![ChatMessage::user("hi")],
            ModelId::new("gemini-1.5-pro"),
            16,
        );
        let err = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(provider.complete(req))
            .unwrap_err();
        assert!(matches!(err, ProviderError::Unauthorized));
    }

    #[test]
    fn finish_reason_mapping() {
        assert!(matches!(
            map_finish_reason(Some("STOP"), Vec::new()),
            FinishReason::Stop
        ));
        assert!(matches!(
            map_finish_reason(Some("MAX_TOKENS"), Vec::new()),
            FinishReason::MaxTokens
        ));
        let calls = vec![ToolCall {
            id: "call_0".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({}),
        }];
        assert!(matches!(
            map_finish_reason(Some("STOP"), calls),
            FinishReason::ToolUse(c) if c.len() == 1
        ));
    }
}
