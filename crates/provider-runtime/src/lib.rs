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
//!   [`Provider::supports_streaming`], and [`Provider::rate_card`].
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
mod error;
mod provider;
mod rate_card;
mod registry;
mod types;

pub use anthropic::AnthropicProvider;
pub use error::ProviderError;
pub use provider::Provider;
pub use rate_card::RateCard;
pub use registry::ProviderRegistry;
pub use types::{
    CompletionRequest, CompletionResponse, CostEnvelope, FinishReason, ModelId, RequestId,
    ToolCall, Usage,
};

// Shared value types owned by §1.0; re-exported so provider and runtime never
// drift into two incompatible schemas.
pub use ardur_runtime::{ChatMessage, CostTuple, ProviderId, Role};
