//! ardur-provider-runtime — the model-provider abstraction the runtime
//! dispatches completions through.
//!
//! Plan family: §3.0 (`plans/3.0-provider-runtime-blueprint.md`); the Anthropic
//! backend follows §3.1 (`plans/3.1-anthropic-provider-blueprint.md`).
//!
//! # Phase 1 (this crate)
//!
//! - [`Provider`] — the object-safe trait every backend implements: an async
//!   [`Provider::complete`], plus [`Provider::id`],
//!   [`Provider::supports_streaming`], and [`Provider::rate_card`]. §3.1b adds
//!   [`Provider::stream`] — an incremental [`StreamEvent`] feed
//!   ([`ProviderStream`]) with a default impl that wraps one `complete()` call,
//!   so a provider opts into real streaming by overriding it.
//! - [`CompletionRequest`] / [`CompletionResponse`] — the call envelopes, with
//!   the [`FinishReason`] taxonomy, [`Usage`] token counts, and the
//!   [`CostEnvelope`] budget ceiling.
//! - [`ProviderRegistry`] — name→provider resolution keyed by [`ProviderId`].
//! - [`AnthropicProvider`] — the Anthropic backend. Phase 1 ships a real
//!   Messages-API HTTP client via [`AnthropicProvider::new`] /
//!   [`AnthropicProvider::from_env`]; [`AnthropicProvider::stub`] retains the
//!   deterministic placeholder for network-free tests.
//! - [`RateCard`] — versioned pricing that turns [`Usage`] into a billed
//!   [`CostTuple`].
//! - [`ProviderError`] — the crate's single typed-error surface.
//! - [`InstrumentedProvider`] — a transparent decorator that wraps each
//!   [`Provider::complete`] in a `provider.send` `tracing` span carrying the
//!   OpenTelemetry GenAI semantic-convention attributes (`gen_ai.*`).
//! - [`telemetry`] — the OTLP exporter + subscriber that ship those spans to any
//!   OTLP backend (Langfuse / Phoenix / Arize / Jaeger), via
//!   [`init_genai_tracing`] / [`shutdown_genai_tracing`].
//!
//! [`ChatMessage`], [`Role`], [`ProviderId`], and [`CostTuple`] are re-exported
//! from `ardur-runtime` so the runtime and the provider layer share one schema
//! for the prompt, the registry key, and the billed cost — rather than
//! redefining placeholders that would later have to be reconciled.
//!
//! Phase 2 (see the inline `// TODO §3.0 Phase 2:` markers) adds streaming,
//! `tool_use` parsing, cost-envelope enforcement at admission, multi-turn cost
//! projection, and request idempotency.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod anthropic;
mod embedding;
mod error;
mod instrument;
mod provider;
mod rate_card;
mod registry;
mod stream;
pub mod telemetry;
mod types;

pub use anthropic::AnthropicProvider;
pub use embedding::{EmbeddingProvider, EmbeddingRequest, EmbeddingResponse};
pub use error::ProviderError;
pub use instrument::InstrumentedProvider;
pub use provider::Provider;
pub use rate_card::RateCard;
pub use registry::{EmbeddingProviderRegistry, ProviderRegistry};
pub use stream::{ProviderStream, StreamEvent};
pub use telemetry::{TelemetryConfig, TelemetryError, init_genai_tracing, shutdown_genai_tracing};
pub use types::{
    CompletionRequest, CompletionResponse, CostEnvelope, FinishReason, ModelId, RequestId, ToolDef,
    Usage,
};

// Shared value types owned by §1.0; re-exported so provider and runtime never
// drift into two incompatible schemas. `ToolCall` is hoisted into `ardur-runtime`
// (so `ChatMessage` can carry it without a cycle) and re-exported here so existing
// `ardur_provider_runtime::ToolCall` references keep resolving.
pub use ardur_runtime::{ChatMessage, CostTuple, ProviderId, Role, ToolCall};
