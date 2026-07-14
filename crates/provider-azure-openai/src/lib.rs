//! ardur-provider-azure-openai — Azure OpenAI Service backend (§3.4).
//!
//! Azure OpenAI speaks the same JSON body dialect as OpenAI Chat Completions,
//! but the transport differs: the URL is scoped to a resource + deployment
//! (`https://{resource}.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={version}`)
//! and auth is an `api-key` header, not `Authorization: Bearer`. The
//! `{deployment}` segment — not a `model` field in the body — selects which
//! model the call runs against, so [`CompletionRequest::model`] is not sent on
//! the wire; it is only the local hint a caller stamps onto the request.
//!
//! # Phase 1 (this crate)
//!
//! - [`AzureOpenAiProvider`] — [`Provider::complete`] against a real Azure
//!   deployment, and [`EmbeddingProvider::embed`] against `/embeddings`.
//! - `supports_streaming()` is `false`: the trait default [`Provider::stream`]
//!   wraps one `complete()` call, matching the non-incremental precedent
//!   `ardur-provider-codex`/`ardur-provider-claude-cli` set. Phase 2 (TODO)
//!   adds a real SSE decode — the wire framing is identical to OpenAI's.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fmt;

use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, EmbeddingProvider, EmbeddingRequest, EmbeddingResponse,
    FinishReason, ModelId, Provider, ProviderError, RateCard, ToolCall, Usage,
};
use ardur_runtime::{ChatMessage, CostTuple, ProviderId, Role};
use async_trait::async_trait;
use serde::Deserialize;

/// The registry key this backend answers to.
const PROVIDER_ID: &str = "azure-openai";
/// Default API version, matching Azure's general-availability Chat
/// Completions surface at the time this backend was written.
pub const DEFAULT_API_VERSION: &str = "2024-10-21";

/// `ARDUR_AZURE_OPENAI_API_KEY` — the Azure resource's API key.
pub const API_KEY_ENV: &str = "ARDUR_AZURE_OPENAI_API_KEY";
/// `ARDUR_AZURE_OPENAI_RESOURCE` — the Azure resource name (the `{resource}`
/// in `https://{resource}.openai.azure.com`).
pub const RESOURCE_ENV: &str = "ARDUR_AZURE_OPENAI_RESOURCE";
/// `ARDUR_AZURE_OPENAI_DEPLOYMENT` — the deployment name that selects the
/// model.
pub const DEPLOYMENT_ENV: &str = "ARDUR_AZURE_OPENAI_DEPLOYMENT";
/// `ARDUR_AZURE_OPENAI_API_VERSION` — optional override; defaults to
/// [`DEFAULT_API_VERSION`].
pub const API_VERSION_ENV: &str = "ARDUR_AZURE_OPENAI_API_VERSION";
/// `ARDUR_AZURE_OPENAI_EMBEDDING_DEPLOYMENT` — optional separate deployment
/// name for [`EmbeddingProvider::embed`]; defaults to [`DEPLOYMENT_ENV`] when
/// unset (Azure resources commonly reuse one deployment name across a chat
/// and an embedding model is uncommon, so most callers set this explicitly).
pub const EMBEDDING_DEPLOYMENT_ENV: &str = "ARDUR_AZURE_OPENAI_EMBEDDING_DEPLOYMENT";

/// How an [`AzureOpenAiProvider`] connects to its Azure resource.
#[derive(Clone)]
pub struct AzureOpenAiConfig {
    api_key: String,
    resource: String,
    deployment: String,
    embedding_deployment: String,
    api_version: String,
    base_url_override: Option<String>,
}

impl fmt::Debug for AzureOpenAiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AzureOpenAiConfig")
            .field("api_key", &redacted_present(&self.api_key))
            .field("resource", &self.resource)
            .field("deployment", &self.deployment)
            .field("embedding_deployment", &self.embedding_deployment)
            .field("api_version", &self.api_version)
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

impl AzureOpenAiConfig {
    /// A config bound to a resource, deployment, and key, with the default API
    /// version and the embedding deployment defaulted to `deployment`.
    pub fn new(
        api_key: impl Into<String>,
        resource: impl Into<String>,
        deployment: impl Into<String>,
    ) -> Self {
        let deployment = deployment.into();
        Self {
            api_key: api_key.into(),
            resource: resource.into(),
            embedding_deployment: deployment.clone(),
            deployment,
            api_version: DEFAULT_API_VERSION.to_string(),
            base_url_override: None,
        }
    }

    /// Read config from environment: [`API_KEY_ENV`], [`RESOURCE_ENV`],
    /// [`DEPLOYMENT_ENV`] are required; [`API_VERSION_ENV`] and
    /// [`EMBEDDING_DEPLOYMENT_ENV`] are optional.
    ///
    /// Returns [`ProviderError::Unauthorized`] when the key is unset, and
    /// [`ProviderError::InvalidRequest`] when the resource or deployment is
    /// unset (neither has a sane default — an empty resource/deployment would
    /// build a malformed URL).
    pub fn from_env() -> Result<Self, ProviderError> {
        let key = nonempty_env(API_KEY_ENV).ok_or(ProviderError::Unauthorized)?;
        let resource = nonempty_env(RESOURCE_ENV)
            .ok_or_else(|| ProviderError::InvalidRequest(format!("{RESOURCE_ENV} must be set")))?;
        let deployment = nonempty_env(DEPLOYMENT_ENV).ok_or_else(|| {
            ProviderError::InvalidRequest(format!("{DEPLOYMENT_ENV} must be set"))
        })?;

        let mut config = Self::new(key, resource, deployment);
        if let Some(version) = nonempty_env(API_VERSION_ENV) {
            config.api_version = version;
        }
        if let Some(embedding_deployment) = nonempty_env(EMBEDDING_DEPLOYMENT_ENV) {
            config.embedding_deployment = embedding_deployment;
        }
        Ok(config)
    }

    /// Override the base URL's scheme+host, for pointing at a mock server in
    /// tests. Production always talks to
    /// `https://{resource}.openai.azure.com`; tests override with
    /// [`AzureOpenAiConfig::base_url_override`].
    #[must_use]
    pub fn base_url_override(mut self, base_url: impl Into<String>) -> Self {
        self.base_url_override = Some(base_url.into());
        self
    }

    fn chat_url(&self) -> String {
        format!(
            "{}/openai/deployments/{}/chat/completions?api-version={}",
            self.base(),
            self.deployment,
            self.api_version
        )
    }

    fn embeddings_url(&self) -> String {
        format!(
            "{}/openai/deployments/{}/embeddings?api-version={}",
            self.base(),
            self.embedding_deployment,
            self.api_version
        )
    }

    fn base(&self) -> String {
        self.base_url_override
            .clone()
            .unwrap_or_else(|| format!("https://{}.openai.azure.com", self.resource))
    }
}

fn nonempty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The Azure OpenAI provider.
pub struct AzureOpenAiProvider {
    config: AzureOpenAiConfig,
    model_id: ModelId,
    rate_card: RateCard,
    embedding_dim: usize,
    client: reqwest::Client,
}

impl AzureOpenAiProvider {
    /// Build a live provider from `config`, defaulting completions to
    /// `model_id` (a local hint only — Azure selects the model via the
    /// deployment in the URL, not a body field).
    #[must_use]
    pub fn new(config: AzureOpenAiConfig, model_id: ModelId) -> Self {
        Self {
            config,
            model_id,
            rate_card: azure_openai_passthrough_rate_card(),
            embedding_dim: 1536, // text-embedding-3-small's dimension; overridable via with_embedding_dim.
            client: reqwest::Client::new(),
        }
    }

    /// Build a live provider reading [`AzureOpenAiConfig::from_env`].
    pub fn from_env(model_id: ModelId) -> Result<Self, ProviderError> {
        Ok(Self::new(AzureOpenAiConfig::from_env()?, model_id))
    }

    /// Override the embedding dimension this provider reports (builder-style)
    /// — the default (1536) matches `text-embedding-3-small`; set this when
    /// the embedding deployment runs a different model (e.g. `-large` at
    /// 3072, or `ada-002` at 1536).
    #[must_use]
    pub fn with_embedding_dim(mut self, dim: usize) -> Self {
        self.embedding_dim = dim;
        self
    }

    /// The model this provider defaults completions to.
    #[must_use]
    pub fn model_id(&self) -> &ModelId {
        &self.model_id
    }
}

#[async_trait]
impl Provider for AzureOpenAiProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        if self.config.api_key.is_empty() {
            return Err(ProviderError::Unauthorized);
        }

        let body = build_request_body(&req);
        let resp = self
            .client
            .post(self.config.chat_url())
            .header("api-key", &self.config.api_key)
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

        let code = status.as_u16();
        let retry_after_ms = parse_retry_after_ms(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        Err(map_http_error(code, retry_after_ms, &body, &req.model))
    }

    fn id(&self) -> ProviderId {
        ProviderId(PROVIDER_ID.to_string())
    }

    fn supports_streaming(&self) -> bool {
        // Phase 2 TODO §3.4: a real incremental SSE decode. Phase 1 relies on
        // the trait default `stream()`, which wraps one `complete()` call.
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

#[async_trait]
impl EmbeddingProvider for AzureOpenAiProvider {
    async fn embed(&self, req: EmbeddingRequest) -> Result<EmbeddingResponse, ProviderError> {
        if self.config.api_key.is_empty() {
            return Err(ProviderError::Unauthorized);
        }
        if req.input.is_empty() {
            return Err(ProviderError::InvalidRequest(
                "embedding request must carry at least one input string".to_string(),
            ));
        }

        let body = serde_json::json!({ "input": req.input });
        let resp = self
            .client
            .post(self.config.embeddings_url())
            .header("api-key", &self.config.api_key)
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
            let parsed: EmbeddingsApiResponse = serde_json::from_value(value.clone())
                .map_err(|e| ProviderError::Upstream(format!("unexpected response shape: {e}")))?;
            return Ok(parsed.into_embedding_response(value));
        }

        let code = status.as_u16();
        let retry_after_ms = parse_retry_after_ms(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        Err(map_http_error(
            code,
            retry_after_ms,
            &body,
            &ModelId::new(req.model),
        ))
    }

    fn id(&self) -> ProviderId {
        ProviderId(PROVIDER_ID.to_string())
    }

    fn embedding_dim(&self) -> usize {
        self.embedding_dim
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// A zeroed "passthrough" rate card — Azure pricing is resource/negotiated,
/// not one public catalog; the card exists only to satisfy
/// [`Provider::rate_card`]/[`EmbeddingProvider::rate_card`].
fn azure_openai_passthrough_rate_card() -> RateCard {
    RateCard {
        version_id: "azure-openai-passthrough-v1".to_string(),
        cents_per_1k_input: 0.0,
        cents_per_1k_output: 0.0,
        cents_per_request: 0.0,
    }
}

/// Serialize a [`CompletionRequest`] into the Azure/OpenAI chat-completions
/// body. Identical dialect to `provider-openai-compat`'s non-streaming body
/// (see that crate's `request_body`), duplicated here per this crate's design
/// note: a resource-scoped Azure client staying independent of the generic
/// OpenAI-compat crate keeps the provider dependency graph a DAG rooted at
/// `ardur-provider-runtime`. `model` is intentionally omitted — Azure selects
/// the model via the deployment in the URL.
fn build_request_body(req: &CompletionRequest) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = req.messages.iter().map(azure_message).collect();

    let mut body = serde_json::json!({
        "messages": messages,
        "max_tokens": req.max_tokens,
        "temperature": req.temperature,
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

fn azure_message(m: &ChatMessage) -> serde_json::Value {
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

fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn parse_retry_after_ms(headers: &reqwest::header::HeaderMap) -> u64 {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|secs| secs.saturating_mul(1000))
        .unwrap_or(0)
}

/// Map a non-2xx Azure OpenAI response onto the crate's [`ProviderError`]
/// taxonomy. Azure's error envelope matches OpenAI's:
/// `{ "error": { "message", "code", … } }`.
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

#[derive(Default)]
struct ApiErrorBody {
    message: Option<String>,
    code: Option<String>,
}

impl ApiErrorBody {
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

    fn describe(&self) -> String {
        match (&self.message, &self.code) {
            (Some(m), Some(c)) => format!("{m} (code: {c})"),
            (Some(m), None) => m.clone(),
            (None, Some(c)) => format!("azure-openai error (code: {c})"),
            (None, None) => "azure-openai error with no message".to_string(),
        }
    }
}

#[derive(Deserialize)]
struct ChatCompletion {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

#[derive(Deserialize)]
struct Choice {
    #[serde(default)]
    message: Message,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct Message {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ApiToolCall>,
}

#[derive(Deserialize)]
struct ApiToolCall {
    #[serde(default)]
    id: String,
    function: ApiToolFunction,
}

#[derive(Deserialize)]
struct ApiToolFunction {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Deserialize)]
struct ApiUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

impl ChatCompletion {
    fn into_completion(self, raw: serde_json::Value) -> CompletionResponse {
        let (content, tool_calls, finish_raw) = match self.choices.into_iter().next() {
            Some(choice) => {
                let content = choice.message.content.clone().unwrap_or_default();
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

        let usage = match self.usage {
            Some(u) => Usage {
                tokens_in: u.prompt_tokens,
                tokens_out: u.completion_tokens,
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

/// The subset of the Azure OpenAI `/embeddings` response this crate reads.
#[derive(Deserialize)]
struct EmbeddingsApiResponse {
    #[serde(default)]
    data: Vec<EmbeddingDatum>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

#[derive(Deserialize)]
struct EmbeddingDatum {
    #[serde(default)]
    embedding: Vec<f32>,
    #[serde(default)]
    index: usize,
}

impl EmbeddingsApiResponse {
    fn into_embedding_response(mut self, raw: serde_json::Value) -> EmbeddingResponse {
        // The API returns `data` in request order, but sort defensively on
        // `index` so a reordered response still lines up with the caller's
        // input order.
        self.data.sort_by_key(|d| d.index);
        let vectors = self.data.into_iter().map(|d| d.embedding).collect();
        let usage = match self.usage {
            Some(u) => Usage {
                tokens_in: u.prompt_tokens,
                tokens_out: 0,
                cost_cents: None,
            },
            None => Usage::default(),
        };
        EmbeddingResponse {
            vectors,
            usage,
            raw_provider_response: Some(raw),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AzureOpenAiConfig {
        AzureOpenAiConfig::new("k", "my-resource", "gpt-4o-deployment")
    }

    #[test]
    fn chat_url_is_resource_and_deployment_scoped() {
        let cfg = config();
        assert_eq!(
            cfg.chat_url(),
            "https://my-resource.openai.azure.com/openai/deployments/gpt-4o-deployment/chat/completions?api-version=2024-10-21"
        );
    }

    #[test]
    fn embeddings_url_defaults_to_chat_deployment() {
        let cfg = config();
        assert_eq!(
            cfg.embeddings_url(),
            "https://my-resource.openai.azure.com/openai/deployments/gpt-4o-deployment/embeddings?api-version=2024-10-21"
        );
    }

    #[test]
    fn request_body_omits_model_field() {
        let req = CompletionRequest::new(vec![ChatMessage::user("hi")], ModelId::new("gpt-4o"), 16);
        let body = build_request_body(&req);
        assert!(
            body.get("model").is_none(),
            "Azure selects the model via the deployment URL, not a body field"
        );
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn debug_redacts_api_key() {
        let cfg = AzureOpenAiConfig::new("DUMMY_KEY_FOR_TESTING_ONLY", "r", "d");
        let rendered = format!("{cfg:?}");
        assert!(
            !rendered.contains("DUMMY_KEY_FOR_TESTING_ONLY"),
            "{rendered}"
        );
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn provider_id_is_azure_openai() {
        let provider = AzureOpenAiProvider::new(config(), ModelId::new("gpt-4o"));
        assert_eq!(
            Provider::id(&provider),
            ProviderId("azure-openai".to_string())
        );
        assert!(!Provider::supports_streaming(&provider));
    }

    #[test]
    fn empty_api_key_is_unauthorized() {
        let provider =
            AzureOpenAiProvider::new(AzureOpenAiConfig::new("", "r", "d"), ModelId::new("gpt-4o"));
        let req = CompletionRequest::new(vec![ChatMessage::user("hi")], ModelId::new("gpt-4o"), 16);
        let err = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(provider.complete(req))
            .unwrap_err();
        assert!(matches!(err, ProviderError::Unauthorized));
    }

    #[test]
    fn empty_embedding_input_is_invalid_request() {
        let provider = AzureOpenAiProvider::new(config(), ModelId::new("gpt-4o"));
        let req = EmbeddingRequest::new(Vec::new(), "text-embedding-3-small");
        let err = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(EmbeddingProvider::embed(&provider, req))
            .unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn http_error_maps_to_taxonomy() {
        let model = ModelId::new("gpt-4o");
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
    }

    #[test]
    fn embedding_response_sorts_by_index() {
        let raw = serde_json::json!({
            "data": [
                {"embedding": [2.0], "index": 1},
                {"embedding": [1.0], "index": 0}
            ],
            "usage": {"prompt_tokens": 5, "completion_tokens": 0}
        });
        let parsed: EmbeddingsApiResponse = serde_json::from_value(raw.clone()).unwrap();
        let resp = parsed.into_embedding_response(raw);
        assert_eq!(resp.vectors, vec![vec![1.0], vec![2.0]]);
        assert_eq!(resp.usage.tokens_in, 5);
    }
}
