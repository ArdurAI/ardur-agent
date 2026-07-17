//! The values a provider call is expressed in: the request/response envelopes,
//! the token/budget accounting types, and the finish-reason taxonomy.
//!
//! [`ChatMessage`](ardur_runtime::ChatMessage),
//! [`Role`](ardur_runtime::Role), and [`CostTuple`](ardur_runtime::CostTuple)
//! are re-exported from `ardur-runtime` (see the crate root) rather than
//! redefined here.

use ardur_runtime::{ChatMessage, ToolCall};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifier of a concrete model offered by a provider (e.g.
/// `"claude-opus-4-8"`). Opaque to this layer — each provider validates it.
///
/// Re-exported from `ardur-core-types` so the provider and cost-gate layers
/// name the one `ModelId` (with the same `new`/`Display` surface).
pub use ardur_core_types::ModelId;

/// Caller-supplied correlation id for a single completion request (UUIDv4).
///
/// It travels with the request so logs, receipts, and retries can be tied back
/// to one logical call.
// TODO §3.0 Phase 2: use this as the idempotency key the HTTP path replays on,
// so a retried request is de-duplicated upstream rather than re-billed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId(pub Uuid);

impl RequestId {
    /// Mint a fresh, random request id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

/// The token counts a provider reports for one completion.
///
/// This is the *raw* usage the provider billed; pricing it into a
/// [`CostTuple`](ardur_runtime::CostTuple) is the [`RateCard`](crate::RateCard)'s
/// job.
///
/// Some providers (e.g. OpenRouter) also report the actual dollar cost of the
/// call on the response. When present, `cost_cents` is the billed cost in whole
/// US cents and takes precedence over rate-card pricing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt/input tokens consumed.
    pub tokens_in: u32,
    /// Completion/output tokens generated.
    pub tokens_out: u32,
    /// The provider-reported actual cost of the call, in whole US cents.
    /// `None` means the provider did not report a cost (most backends).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_cents: Option<u64>,
}

/// The cost ceiling a caller is willing to spend on a request.
///
/// A `None` field means that dimension is unbounded. Phase 1 records the
/// envelope but does not enforce it.
// TODO §3.0 Phase 2: enforce the envelope at admission — price the projected
// usage against the rate card and reject with
// [`ProviderError::CostCeilingExceeded`](crate::ProviderError::CostCeilingExceeded)
// before dispatching the upstream call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostEnvelope {
    /// Hard ceiling on monetary cost, in whole US cents.
    pub max_cents: Option<u64>,
    /// Hard ceiling on total (input + output) tokens.
    pub max_total_tokens: Option<u64>,
}

impl CostEnvelope {
    /// An envelope with no ceiling on any dimension.
    #[must_use]
    pub fn unbounded() -> Self {
        Self::default()
    }
}

/// A tool advertised to the provider so the model may request it (surfacing as
/// [`FinishReason::ToolUse`]).
///
/// The runtime builds one per registered tool from its tool-registry schema and
/// threads them on [`CompletionRequest::tools`]; a provider that supports tool
/// use serializes them into its wire format (Anthropic's `tools`, the
/// OpenAI-compatible `tools`). An empty `tools` array (the default) reproduces a
/// no-tool request byte-for-byte, so a provider that never sees a tool behaves
/// exactly as before this field existed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    /// The tool's stable name (its registry id), echoed back in a
    /// [`ToolCall`](ardur_runtime::ToolCall).
    pub name: String,
    /// A one-line, model-facing description of what the tool does.
    pub description: String,
    /// JSON Schema of the arguments the tool accepts.
    pub input_schema: serde_json::Value,
}

/// Why a completion stopped generating.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum FinishReason {
    /// The model emitted a natural end-of-turn.
    Stop,
    /// Generation hit the request's `max_tokens` ceiling.
    MaxTokens,
    /// One of the request's `stop_sequences` was produced (carries which one).
    StopSequence(String),
    /// The model is requesting one or more tool calls.
    ToolUse(Vec<ToolCall>),
    /// Generation aborted with a provider-reported error (carries the message).
    Error(String),
}

/// One completion request handed to a [`Provider`](crate::Provider).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// Correlation id for logs, receipts, and retries.
    pub request_id: RequestId,
    /// The prompt: an ordered chat transcript.
    pub messages: Vec<ChatMessage>,
    /// The model to run the completion against.
    pub model: ModelId,
    /// Upper bound on output tokens to generate.
    pub max_tokens: u32,
    /// Sampling temperature.
    pub temperature: f32,
    /// Sequences that, if generated, halt the completion.
    pub stop_sequences: Vec<String>,
    /// The cost ceiling the caller authorizes for this request.
    pub requested_cost_envelope: CostEnvelope,
    /// Tools the model may request on this call. Empty (the default) means no
    /// tools are advertised, and the serialized request is identical to one
    /// built before this field existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    /// Whether the caller prefers incremental delivery via
    /// [`Provider::stream`](crate::Provider::stream). A dispatcher reads this to
    /// choose `stream()` over [`complete`](crate::Provider::complete) when the
    /// provider [`supports_streaming`](crate::Provider::supports_streaming); it
    /// never changes the non-streaming wire body. `false` (the default) is
    /// skipped on serialization, so a request built before this field existed
    /// round-trips byte-for-byte.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stream: bool,
}

/// `skip_serializing_if` predicate keeping a `false` `stream` flag out of the
/// serialized [`CompletionRequest`], so stored receipts/journals stay identical
/// to the pre-§3.1b shape.
fn is_false(b: &bool) -> bool {
    !*b
}

impl CompletionRequest {
    /// Build a request against `model` with a fresh [`RequestId`], an unbounded
    /// [`CostEnvelope`], `temperature` 1.0, no stop sequences, and no tools.
    pub fn new(messages: Vec<ChatMessage>, model: ModelId, max_tokens: u32) -> Self {
        Self {
            request_id: RequestId::new(),
            messages,
            model,
            max_tokens,
            temperature: 1.0,
            stop_sequences: Vec::new(),
            requested_cost_envelope: CostEnvelope::unbounded(),
            tools: Vec::new(),
            stream: false,
        }
    }

    /// Advertise `tools` to the provider on this request (builder-style).
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<ToolDef>) -> Self {
        self.tools = tools;
        self
    }

    /// Mark this request as preferring streamed delivery (builder-style). Sets
    /// the [`stream`](Self::stream) hint a dispatcher reads to pick
    /// [`Provider::stream`](crate::Provider::stream).
    #[must_use]
    pub fn streaming(mut self) -> Self {
        self.stream = true;
        self
    }
}

/// One completion result returned by a [`Provider`](crate::Provider).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompletionResponse {
    /// The generated text.
    pub content: String,
    /// Why generation stopped.
    pub finish_reason: FinishReason,
    /// Raw token counts the provider billed.
    pub usage: Usage,
    /// The rate-card-priced cost of the call.
    pub cost: ardur_runtime::CostTuple,
    /// The untouched upstream response body, when retained for audit.
    pub raw_provider_response: Option<serde_json::Value>,
}
