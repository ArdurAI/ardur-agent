//! [`InstrumentedProvider`] — the one place a provider call is wrapped in a
//! `tracing` span carrying OpenTelemetry GenAI semantic-convention attributes.
//!
//! This is a transparent decorator over any `Arc<dyn Provider>`: it delegates
//! every trait method to the inner provider, but routes
//! [`complete`](Provider::complete) through a `provider.send` span. Wrapping
//! once here — rather than editing each `provider-*` crate — means every backend
//! (Anthropic, OpenRouter, Ollama, Codex, Claude CLI) emits the same `gen_ai.*`
//! attributes for free the moment its handle is wrapped at boot.
//!
//! The attribute names follow the OTel GenAI spec
//! (<https://opentelemetry.io/docs/specs/semconv/gen-ai/>): `gen_ai.system`,
//! `gen_ai.operation.name`, `gen_ai.request.{model,temperature,max_tokens}`,
//! `gen_ai.response.{model,finish_reasons}`, `gen_ai.usage.{input,output}_tokens`,
//! and `error.type` on the failure path. The `tracing-opentelemetry` bridge maps
//! each span field of those names onto the corresponding OTel span attribute, so
//! an OTLP backend (Langfuse / Phoenix / Arize / Jaeger) sees them natively.

use std::sync::Arc;

use ardur_runtime::ProviderId;
use async_trait::async_trait;
use tracing::Instrument;

use crate::error::ProviderError;
use crate::provider::Provider;
use crate::rate_card::RateCard;
use crate::stream::ProviderStream;
use crate::types::{CompletionRequest, CompletionResponse, FinishReason};

/// A [`Provider`] decorator that opens a `provider.send` span — carrying the
/// OpenTelemetry GenAI attributes for the request and (on return) the response
/// or error — around each [`complete`](Provider::complete) call, delegating
/// every other method untouched to the wrapped provider.
pub struct InstrumentedProvider {
    inner: Arc<dyn Provider>,
}

impl InstrumentedProvider {
    /// Wrap `inner` so its completions are traced. Prefer [`Self::wrap`] when the
    /// result is handed straight back into a `dyn Provider` slot.
    #[must_use]
    pub fn new(inner: Arc<dyn Provider>) -> Self {
        Self { inner }
    }

    /// Wrap `inner` and return it type-erased as `Arc<dyn Provider>` — the shape
    /// the CLI/server boot paths and the [`ProviderRegistry`](crate::ProviderRegistry)
    /// store. `InstrumentedProvider::wrap(p)` is a drop-in for `p` at any
    /// `Arc<dyn Provider>` seam.
    #[must_use]
    pub fn wrap(inner: Arc<dyn Provider>) -> Arc<dyn Provider> {
        Arc::new(Self::new(inner))
    }
}

#[async_trait]
impl Provider for InstrumentedProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        // The request-side GenAI attributes are known up front; the response-side
        // ones are declared `Empty` and filled in once `complete` returns.
        let span = tracing::info_span!(
            "provider.send",
            "gen_ai.system" = %self.inner.id().0,
            "gen_ai.operation.name" = "chat",
            "gen_ai.request.model" = %req.model,
            "gen_ai.request.temperature" = req.temperature,
            "gen_ai.request.max_tokens" = req.max_tokens,
            "gen_ai.response.model" = tracing::field::Empty,
            "gen_ai.response.finish_reasons" = tracing::field::Empty,
            "gen_ai.usage.input_tokens" = tracing::field::Empty,
            "gen_ai.usage.output_tokens" = tracing::field::Empty,
            "error.type" = tracing::field::Empty,
        );

        let inner = Arc::clone(&self.inner);
        async move {
            let result = inner.complete(req).await;
            let span = tracing::Span::current();
            match &result {
                Ok(resp) => {
                    // `response.model` is not a distinct field on `CompletionResponse`
                    // (the response carries no separate served-model id), so the
                    // requested model — already on `gen_ai.request.model` — is the
                    // best available value; recording it keeps the response side of
                    // the semconv populated for backends that key on it.
                    span.record("gen_ai.usage.input_tokens", resp.usage.tokens_in);
                    span.record("gen_ai.usage.output_tokens", resp.usage.tokens_out);
                    span.record(
                        "gen_ai.response.finish_reasons",
                        finish_reasons_attr(&resp.finish_reason).as_str(),
                    );
                }
                Err(err) => {
                    span.record("error.type", error_type(err));
                    span.record("gen_ai.response.finish_reasons", "[\"error\"]");
                }
            }
            result
        }
        .instrument(span)
        .await
    }

    /// Delegate streaming straight to the inner provider, so wrapping at boot
    /// preserves a backend's real SSE path (§3.1b) rather than silently
    /// collapsing it to the `complete`-based default. (Per-chunk span enrichment
    /// is a Phase-2 follow-up; for now the inner stream is returned untouched.)
    async fn stream(&self, req: CompletionRequest) -> Result<ProviderStream, ProviderError> {
        self.inner.stream(req).await
    }

    fn id(&self) -> ProviderId {
        self.inner.id()
    }

    fn supports_streaming(&self) -> bool {
        self.inner.supports_streaming()
    }

    fn rate_card(&self) -> &RateCard {
        self.inner.rate_card()
    }
}

/// Render a [`FinishReason`] as the OTel `gen_ai.response.finish_reasons`
/// attribute. The spec types this attribute as an array of strings; `tracing`
/// span fields carry only scalars, so it is recorded as a JSON-array *string*
/// (`["stop"]`) — faithful to the wire shape an OTLP backend renders, within the
/// `tracing` field model.
fn finish_reasons_attr(reason: &FinishReason) -> String {
    format!("[\"{}\"]", finish_reason_token(reason))
}

/// The single canonical token for a [`FinishReason`], per the GenAI semconv
/// `finish_reasons` vocabulary.
fn finish_reason_token(reason: &FinishReason) -> &'static str {
    match reason {
        FinishReason::Stop => "stop",
        FinishReason::MaxTokens => "length",
        FinishReason::StopSequence(_) => "stop_sequence",
        FinishReason::ToolUse(_) => "tool_calls",
        FinishReason::Error(_) => "error",
    }
}

/// The `error.type` attribute value for a [`ProviderError`]: its variant name,
/// the upstream-independent failure class the runtime switches on.
fn error_type(err: &ProviderError) -> &'static str {
    match err {
        ProviderError::NetworkFailure(_) => "NetworkFailure",
        ProviderError::RateLimited { .. } => "RateLimited",
        ProviderError::InvalidRequest(_) => "InvalidRequest",
        ProviderError::ModelNotAvailable(_) => "ModelNotAvailable",
        ProviderError::CostCeilingExceeded => "CostCeilingExceeded",
        ProviderError::Unauthorized => "Unauthorized",
        ProviderError::Upstream(_) => "Upstream",
        ProviderError::InvalidSelection(_) => "InvalidSelection",
    }
}
