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

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use ardur_runtime::ProviderId;
use async_trait::async_trait;
use futures::Stream;
use tracing::Instrument;

use crate::error::ProviderError;
use crate::provider::Provider;
use crate::rate_card::RateCard;
use crate::stream::{ProviderStream, StreamEvent};
use crate::types::{CompletionRequest, CompletionResponse, FinishReason, ModelId, Usage};

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
            "gen_ai.usage.cost_cents" = tracing::field::Empty,
            "error.type" = tracing::field::Empty,
        );

        let inner = Arc::clone(&self.inner);
        let requested_model = req.model.clone();
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
                    let response_model = response_model_attr(resp, &requested_model);
                    span.record("gen_ai.response.model", response_model.as_str());
                    record_usage(&span, resp.usage, resp.cost.cents);
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

    /// Preserve the backend's real streaming path while keeping the
    /// `provider.send` span open for the lifetime of the returned stream. The
    /// wrapper records final usage, cost, finish reason, and mid-stream errors as
    /// events are polled; entering the span around every poll keeps provider
    /// internals nested under the GenAI span in tracing/OTel backends.
    async fn stream(&self, req: CompletionRequest) -> Result<ProviderStream, ProviderError> {
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
            "gen_ai.usage.cost_cents" = tracing::field::Empty,
            "error.type" = tracing::field::Empty,
        );

        let inner = Arc::clone(&self.inner);
        let requested_model = req.model.clone();
        let rate_card = self.inner.rate_card().clone();
        let result = async move { inner.stream(req).await }
            .instrument(span.clone())
            .await;

        match result {
            Ok(stream) => {
                // Record the requested model as the initial `gen_ai.response.model`.
                // If the stream emits a `ServedModel` event (carrying the actual
                // model the provider served), the wrapper overwrites this with
                // the real value. This is the streaming analogue of the
                // `response_model_attr()` fallback used in the `complete()` path.
                span.record("gen_ai.response.model", requested_model.0.as_str());
                Ok(Box::pin(InstrumentedProviderStream::new(
                    stream, rate_card, span,
                )))
            }
            Err(err) => {
                span.record("error.type", error_type(&err));
                span.record("gen_ai.response.finish_reasons", "[\"error\"]");
                Err(err)
            }
        }
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

/// A stream wrapper that keeps the GenAI `provider.send` span alive until the
/// caller drains or drops the stream, and enriches it from terminal stream
/// events. The inner stream is already pinned inside [`ProviderStream`], so the
/// wrapper itself can be safely moved.
struct InstrumentedProviderStream {
    inner: ProviderStream,
    rate_card: RateCard,
    span: tracing::Span,
    saw_error: bool,
}

impl InstrumentedProviderStream {
    fn new(inner: ProviderStream, rate_card: RateCard, span: tracing::Span) -> Self {
        Self {
            inner,
            rate_card,
            span,
            saw_error: false,
        }
    }
}

impl Unpin for InstrumentedProviderStream {}

impl Stream for InstrumentedProviderStream {
    type Item = Result<StreamEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        let poll = {
            let _entered = this.span.enter();
            this.inner.as_mut().poll_next(cx)
        };

        if let Poll::Ready(Some(item)) = &poll {
            match item {
                Ok(StreamEvent::Usage(usage)) if !this.saw_error => {
                    let priced = this.rate_card.price(*usage);
                    record_usage(&this.span, *usage, priced.cents);
                }
                Ok(StreamEvent::ServedModel(model)) if !this.saw_error => {
                    this.span.record("gen_ai.response.model", model.as_str());
                }
                Ok(StreamEvent::Finish(reason)) if !this.saw_error => {
                    this.span.record(
                        "gen_ai.response.finish_reasons",
                        finish_reasons_attr(reason).as_str(),
                    );
                }
                Err(err) => {
                    this.saw_error = true;
                    this.span.record("error.type", error_type(err));
                    this.span
                        .record("gen_ai.response.finish_reasons", "[\"error\"]");
                }
                Ok(StreamEvent::ContentDelta(_))
                | Ok(StreamEvent::ToolCallStart(_))
                | Ok(StreamEvent::ToolCallDelta { .. })
                | Ok(StreamEvent::Usage(_))
                | Ok(StreamEvent::Finish(_))
                | Ok(StreamEvent::ServedModel(_)) => {}
            }
        }

        poll
    }
}

fn record_usage(span: &tracing::Span, usage: Usage, cost_cents: u64) {
    span.record("gen_ai.usage.input_tokens", usage.tokens_in);
    span.record("gen_ai.usage.output_tokens", usage.tokens_out);
    span.record("gen_ai.usage.cost_cents", cost_cents);
}

fn response_model_attr(resp: &CompletionResponse, requested_model: &ModelId) -> String {
    resp.raw_provider_response
        .as_ref()
        .and_then(|raw| raw.get("model"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or(requested_model.0.as_str())
        .to_string()
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
        ProviderError::InvalidSelection(_) => "InvalidSelection",
        ProviderError::UnknownProvider { .. } => "UnknownProvider",
        ProviderError::CostCeilingExceeded => "CostCeilingExceeded",
        ProviderError::Unauthorized => "Unauthorized",
        ProviderError::Upstream(_) => "Upstream",
    }
}
