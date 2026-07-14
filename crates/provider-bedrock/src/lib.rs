//! ardur-provider-bedrock — AWS Bedrock backend for Anthropic Claude models
//! (§3.4).
//!
//! Targets Bedrock Runtime's `InvokeModel` REST API
//! (`POST https://bedrock-runtime.{region}.amazonaws.com/model/{model_id}/invoke`)
//! for `anthropic.claude-*` model ids, which accept the same Messages-API
//! body Anthropic's direct API does (minus the top-level `model` field —
//! Bedrock selects the model via the URL path — plus a required
//! `anthropic_version` field). Authenticated with the hand-rolled [`sigv4`]
//! signer.
//!
//! # Phase 1 (this crate)
//!
//! - [`BedrockProvider`] — [`Provider::complete`] against a real Bedrock
//!   `InvokeModel` call.
//! - `supports_streaming()` is `true`: [`Provider::stream`] calls
//!   `InvokeModelWithResponseStream` and decodes its binary
//!   `application/vnd.amazon.eventstream` framing (not SSE) into the shared
//!   [`StreamEvent`](ardur_provider_runtime::StreamEvent) protocol — see
//!   [`eventstream`] (the generic frame decoder) and [`streaming`] (the
//!   Bedrock/Anthropic-event adapter).
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fmt;

use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, ModelId, Provider, ProviderError,
    ProviderStream, RateCard, ToolCall, Usage,
};
use ardur_runtime::{ChatMessage, CostTuple, ProviderId, Role};
use async_trait::async_trait;
use serde::Deserialize;

mod eventstream;
mod sigv4;
mod streaming;

/// The Anthropic-on-Bedrock request body's required version tag — distinct
/// from Anthropic's direct-API `anthropic-version` header value.
const ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";
/// The registry key this backend answers to.
const PROVIDER_ID: &str = "bedrock";
/// The default Claude model id on Bedrock.
pub const DEFAULT_MODEL_ID: &str = "anthropic.claude-3-5-sonnet-20241022-v2:0";

/// `AWS_ACCESS_KEY_ID`.
pub const ACCESS_KEY_ID_ENV: &str = "AWS_ACCESS_KEY_ID";
/// `AWS_SECRET_ACCESS_KEY`.
pub const SECRET_ACCESS_KEY_ENV: &str = "AWS_SECRET_ACCESS_KEY";
/// `AWS_SESSION_TOKEN` — optional, for STS-issued temporary credentials.
pub const SESSION_TOKEN_ENV: &str = "AWS_SESSION_TOKEN";
/// `AWS_REGION` — optional, defaults to [`DEFAULT_REGION`].
pub const REGION_ENV: &str = "AWS_REGION";
/// `ARDUR_BEDROCK_MODEL_ID` — optional, defaults to [`DEFAULT_MODEL_ID`].
pub const MODEL_ID_ENV: &str = "ARDUR_BEDROCK_MODEL_ID";
/// The default AWS region when [`REGION_ENV`] is unset.
pub const DEFAULT_REGION: &str = "us-east-1";

/// How a [`BedrockProvider`] authenticates and connects.
#[derive(Clone)]
pub struct BedrockConfig {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    region: String,
    base_url_override: Option<String>,
}

impl fmt::Debug for BedrockConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BedrockConfig")
            .field("access_key_id", &redacted_present(&self.access_key_id))
            .field(
                "secret_access_key",
                &redacted_present(&self.secret_access_key),
            )
            .field(
                "session_token",
                &self.session_token.as_deref().map(redacted_present),
            )
            .field("region", &self.region)
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

impl BedrockConfig {
    /// A config bound to an access key pair, with the default region and no
    /// session token.
    pub fn new(access_key_id: impl Into<String>, secret_access_key: impl Into<String>) -> Self {
        Self {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            session_token: None,
            region: DEFAULT_REGION.to_string(),
            base_url_override: None,
        }
    }

    /// Read config from environment: [`ACCESS_KEY_ID_ENV`] and
    /// [`SECRET_ACCESS_KEY_ENV`] are required; [`SESSION_TOKEN_ENV`] and
    /// [`REGION_ENV`] are optional. Returns [`ProviderError::Unauthorized`]
    /// when either required credential is unset.
    pub fn from_env() -> Result<Self, ProviderError> {
        let access_key_id = nonempty_env(ACCESS_KEY_ID_ENV).ok_or(ProviderError::Unauthorized)?;
        let secret_access_key =
            nonempty_env(SECRET_ACCESS_KEY_ENV).ok_or(ProviderError::Unauthorized)?;

        let mut config = Self::new(access_key_id, secret_access_key);
        config.session_token = nonempty_env(SESSION_TOKEN_ENV);
        if let Some(region) = nonempty_env(REGION_ENV) {
            config.region = region;
        }
        Ok(config)
    }

    /// Override the scheme+host, for pointing at a mock server in tests.
    #[must_use]
    pub fn base_url_override(mut self, base_url: impl Into<String>) -> Self {
        self.base_url_override = Some(base_url.into());
        self
    }

    fn host(&self) -> String {
        format!("bedrock-runtime.{}.amazonaws.com", self.region)
    }

    fn base(&self) -> String {
        self.base_url_override
            .clone()
            .unwrap_or_else(|| format!("https://{}", self.host()))
    }

    fn invoke_url(&self, model_id: &str) -> (String, String) {
        let canonical_uri = format!("/model/{}/invoke", sigv4::uri_encode(model_id));
        (format!("{}{canonical_uri}", self.base()), canonical_uri)
    }

    /// The `InvokeModelWithResponseStream` URL + SigV4 canonical URI for
    /// `model_id`.
    fn invoke_with_response_stream_url(&self, model_id: &str) -> (String, String) {
        let canonical_uri = format!(
            "/model/{}/invoke-with-response-stream",
            sigv4::uri_encode(model_id)
        );
        (format!("{}{canonical_uri}", self.base()), canonical_uri)
    }
}

fn nonempty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The AWS Bedrock provider.
pub struct BedrockProvider {
    config: BedrockConfig,
    model_id: ModelId,
    rate_card: RateCard,
    client: reqwest::Client,
}

impl BedrockProvider {
    /// Build a live provider from `config`, defaulting completions to
    /// `model_id` (the Bedrock model id, e.g.
    /// `"anthropic.claude-3-5-sonnet-20241022-v2:0"`).
    #[must_use]
    pub fn new(config: BedrockConfig, model_id: ModelId) -> Self {
        Self {
            config,
            model_id,
            rate_card: bedrock_passthrough_rate_card(),
            client: reqwest::Client::new(),
        }
    }

    /// Build a live provider reading [`BedrockConfig::from_env`], defaulting
    /// completions to [`MODEL_ID_ENV`] (or [`DEFAULT_MODEL_ID`] when unset).
    pub fn from_env() -> Result<Self, ProviderError> {
        let model_id = ModelId::new(
            nonempty_env(MODEL_ID_ENV).unwrap_or_else(|| DEFAULT_MODEL_ID.to_string()),
        );
        Ok(Self::new(BedrockConfig::from_env()?, model_id))
    }

    /// The model this provider defaults completions to.
    #[must_use]
    pub fn model_id(&self) -> &ModelId {
        &self.model_id
    }

    /// Build a SigV4-signed `POST {url}` request carrying `body_bytes`,
    /// shared by [`Provider::complete`] and [`Provider::stream`] — both sign
    /// the same way, only the target URL and canonical URI (and, upstream,
    /// how the response body is framed) differ.
    fn signed_post(
        &self,
        url: &str,
        canonical_uri: &str,
        body_bytes: &[u8],
    ) -> reqwest::RequestBuilder {
        let credentials = sigv4::Credentials {
            access_key_id: &self.config.access_key_id,
            secret_access_key: &self.config.secret_access_key,
            session_token: self.config.session_token.as_deref(),
        };
        let signed = sigv4::sign(
            &credentials,
            &self.config.region,
            &self.config.host(),
            canonical_uri,
            body_bytes,
            chrono::Utc::now(),
        );

        let mut request = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .header("x-amz-date", &signed.amz_date)
            .header("authorization", &signed.authorization);
        if let Some(token) = &self.config.session_token {
            request = request.header("x-amz-security-token", token);
        }
        request
    }
}

#[async_trait]
impl Provider for BedrockProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        if self.config.access_key_id.is_empty() || self.config.secret_access_key.is_empty() {
            return Err(ProviderError::Unauthorized);
        }

        let body = build_request_body(&req);
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| ProviderError::InvalidRequest(format!("encoding request body: {e}")))?;

        let (url, canonical_uri) = self.config.invoke_url(&req.model.0);
        let resp = self
            .signed_post(&url, &canonical_uri, &body_bytes)
            .body(body_bytes)
            .send()
            .await
            .map_err(|e| ProviderError::NetworkFailure(e.to_string()))?;

        let status = resp.status();
        if status.is_success() {
            let value: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| ProviderError::Upstream(format!("decoding response body: {e}")))?;
            let parsed: InvokeModelResponse = serde_json::from_value(value.clone())
                .map_err(|e| ProviderError::Upstream(format!("unexpected response shape: {e}")))?;
            return Ok(parsed.into_completion(value));
        }

        let code = status.as_u16();
        let body = resp.text().await.unwrap_or_default();
        Err(map_http_error(code, &body, &req.model))
    }

    /// Stream the completion as shared [`StreamEvent`](ardur_provider_runtime::StreamEvent)s
    /// via [`streaming::into_provider_events`], calling
    /// `InvokeModelWithResponseStream`. A connect failure or non-2xx status
    /// is the `Err` of the returned `Result` (resolved before any event
    /// yields); a mid-stream frame-decode error, transport error, or
    /// Bedrock-reported exception is an `Err` item. Cancellation is by drop.
    async fn stream(&self, req: CompletionRequest) -> Result<ProviderStream, ProviderError> {
        if self.config.access_key_id.is_empty() || self.config.secret_access_key.is_empty() {
            return Err(ProviderError::Unauthorized);
        }

        let body = build_request_body(&req);
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| ProviderError::InvalidRequest(format!("encoding request body: {e}")))?;

        let (url, canonical_uri) = self.config.invoke_with_response_stream_url(&req.model.0);
        let resp = self
            .signed_post(&url, &canonical_uri, &body_bytes)
            .body(body_bytes)
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

/// A zeroed "passthrough" rate card — Bedrock pricing is region/model tiered
/// and not one public catalog at this layer; the card exists only to satisfy
/// [`Provider::rate_card`].
fn bedrock_passthrough_rate_card() -> RateCard {
    RateCard {
        version_id: "bedrock-passthrough-v1".to_string(),
        cents_per_1k_input: 0.0,
        cents_per_1k_output: 0.0,
        cents_per_request: 0.0,
    }
}

/// Serialize a [`CompletionRequest`] into the Anthropic-on-Bedrock
/// `InvokeModel` body: the same Messages-API shape Anthropic's direct API
/// uses, minus the top-level `model` (Bedrock selects the model via the URL
/// path), plus the required `anthropic_version`.
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
        "anthropic_version": ANTHROPIC_VERSION,
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

/// Map a non-2xx Bedrock response onto the crate's [`ProviderError`]
/// taxonomy. Bedrock's error envelope carries `message` at the top level
/// (not nested under `error` like Anthropic's direct API).
fn map_http_error(code: u16, body: &str, model: &ModelId) -> ProviderError {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.to_string());
    match code {
        401 | 403 => ProviderError::Unauthorized,
        429 => ProviderError::RateLimited { retry_after_ms: 0 },
        400 => ProviderError::InvalidRequest(message),
        404 => ProviderError::ModelNotAvailable(model.clone()),
        _ => ProviderError::Upstream(format!("HTTP {code}: {message}")),
    }
}

/// The subset of the Anthropic-on-Bedrock `InvokeModel` response this crate
/// reads — the same content-block/`stop_reason`/`usage` shape Anthropic's
/// direct Messages API returns.
#[derive(Deserialize)]
struct InvokeModelResponse {
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Deserialize)]
struct ApiUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

impl InvokeModelResponse {
    fn into_completion(self, raw: serde_json::Value) -> CompletionResponse {
        let mut content = String::new();
        let mut tool_calls = Vec::new();
        for block in self.content {
            match block {
                ContentBlock::Text { text } => content.push_str(&text),
                ContentBlock::ToolUse { id, name, input } => tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: input,
                }),
            }
        }

        let finish_reason = map_stop_reason(self.stop_reason.as_deref(), tool_calls);

        let usage = match self.usage {
            Some(u) => Usage {
                tokens_in: u.input_tokens,
                tokens_out: u.output_tokens,
                cost_cents: None,
            },
            None => Usage::default(),
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

fn map_stop_reason(reason: Option<&str>, tool_calls: Vec<ToolCall>) -> FinishReason {
    match reason {
        Some("end_turn") | Some("stop_sequence") | None => FinishReason::Stop,
        Some("max_tokens") => FinishReason::MaxTokens,
        Some("tool_use") => FinishReason::ToolUse(tool_calls),
        Some(other) => FinishReason::Error(format!("unknown stop_reason: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BedrockConfig {
        BedrockConfig::new("AKIDEXAMPLE", "secret")
    }

    #[test]
    fn invoke_url_percent_encodes_the_model_id() {
        let (url, canonical_uri) = config().invoke_url("anthropic.claude-3-5-sonnet-20241022-v2:0");
        assert_eq!(
            url,
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/invoke"
        );
        assert_eq!(
            canonical_uri,
            "/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/invoke"
        );
    }

    #[test]
    fn request_body_omits_model_and_sets_anthropic_version() {
        let req = CompletionRequest::new(vec![ChatMessage::user("hi")], ModelId::new("m"), 16);
        let body = build_request_body(&req);
        assert!(body.get("model").is_none());
        assert_eq!(body["anthropic_version"], "bedrock-2023-05-31");
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn debug_redacts_credentials() {
        let cfg = BedrockConfig::new("AKID", "SECRET_DUMMY_FOR_TESTING");
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("SECRET_DUMMY_FOR_TESTING"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn provider_id_is_bedrock() {
        let provider = BedrockProvider::new(config(), ModelId::new(DEFAULT_MODEL_ID));
        assert_eq!(provider.id(), ProviderId("bedrock".to_string()));
        assert!(provider.supports_streaming());
    }

    #[test]
    fn missing_credentials_is_unauthorized() {
        let provider =
            BedrockProvider::new(BedrockConfig::new("", ""), ModelId::new(DEFAULT_MODEL_ID));
        let req = CompletionRequest::new(vec![ChatMessage::user("hi")], ModelId::new("m"), 16);
        let err = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(provider.complete(req))
            .unwrap_err();
        assert!(matches!(err, ProviderError::Unauthorized));
    }

    #[test]
    fn stop_reason_mapping() {
        assert!(matches!(
            map_stop_reason(Some("end_turn"), Vec::new()),
            FinishReason::Stop
        ));
        assert!(matches!(
            map_stop_reason(Some("max_tokens"), Vec::new()),
            FinishReason::MaxTokens
        ));
        let calls = vec![ToolCall {
            id: "1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({}),
        }];
        assert!(matches!(
            map_stop_reason(Some("tool_use"), calls),
            FinishReason::ToolUse(c) if c.len() == 1
        ));
    }

    #[test]
    fn http_error_maps_to_taxonomy() {
        let model = ModelId::new("m");
        assert!(matches!(
            map_http_error(401, "{}", &model),
            ProviderError::Unauthorized
        ));
        assert!(matches!(
            map_http_error(404, "{}", &model),
            ProviderError::ModelNotAvailable(_)
        ));
        assert!(matches!(
            map_http_error(400, r#"{"message":"bad request"}"#, &model),
            ProviderError::InvalidRequest(_)
        ));
    }
}
