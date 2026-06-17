//! Integration test for the OpenTelemetry GenAI span emission.
//!
//! [`InstrumentedProvider`] wraps a fake provider; an in-memory
//! `tracing-subscriber` layer captures the `provider.send` span it opens, and we
//! assert the span carries the GenAI semantic-convention attributes for both
//! complete and streaming calls. No OTLP collector is involved — the `tracing`
//! field values the OTel bridge would forward are asserted directly off the
//! captured span.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, InstrumentedProvider, ModelId, Provider,
    ProviderError, ProviderStream, RateCard, StreamEvent, Usage,
};
use ardur_runtime::{ChatMessage, CostTuple, ProviderId};
use async_trait::async_trait;
use futures::{Stream, StreamExt};
use tracing::field::{Field, Visit};
use tracing::subscriber::with_default;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

// ---------------------------------------------------------------------------
// In-memory span capture
// ---------------------------------------------------------------------------

/// One captured span: its name, optional parent span name, and flattened field
/// map (values stringified the way the OTel bridge would serialize them).
#[derive(Clone, Debug, Default)]
struct CapturedSpan {
    name: String,
    parent_name: Option<String>,
    fields: HashMap<String, String>,
}

/// A `tracing` layer that records every span's fields (at creation and on later
/// `record` calls) and pushes the completed span to a shared buffer on close.
#[derive(Clone, Default)]
struct CaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
}

/// Flattens `tracing` field values into clean strings: `record_str` keeps the
/// raw string; the numeric/bool recorders use their natural rendering; the
/// `Debug` fallback (which `%`/Display fields route through) is unquoted.
struct FieldVisitor<'a>(&'a mut HashMap<String, String>);

impl Visit for FieldVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

impl<S> Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::Id,
        ctx: Context<'_, S>,
    ) {
        let span = ctx.span(id).expect("span exists on creation");
        let mut fields = HashMap::new();
        attrs.record(&mut FieldVisitor(&mut fields));
        let parent_name = attrs
            .parent()
            .and_then(|parent| ctx.span(parent))
            .or_else(|| ctx.current_span().id().and_then(|parent| ctx.span(parent)))
            .map(|parent| parent.name().to_string());
        span.extensions_mut().insert(CapturedSpan {
            name: span.name().to_string(),
            parent_name,
            fields,
        });
    }

    fn on_record(&self, id: &tracing::Id, values: &tracing::span::Record<'_>, ctx: Context<'_, S>) {
        let span = ctx.span(id).expect("span exists on record");
        let mut ext = span.extensions_mut();
        if let Some(captured) = ext.get_mut::<CapturedSpan>() {
            values.record(&mut FieldVisitor(&mut captured.fields));
        }
    }

    fn on_close(&self, id: tracing::Id, ctx: Context<'_, S>) {
        let span = ctx.span(&id).expect("span exists on close");
        let captured = span.extensions().get::<CapturedSpan>().cloned();
        if let Some(captured) = captured {
            self.spans.lock().expect("span buffer lock").push(captured);
        }
    }
}

// ---------------------------------------------------------------------------
// Fake provider that records complete and stream spans deterministically
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum FakeMode {
    CompleteOk {
        raw_response_model: Option<&'static str>,
        cost_cents: u64,
    },
    CompleteErr,
    StreamOk,
    StreamStartErr,
    StreamMidErr,
}

/// A provider that returns scripted results, so span attributes are
/// deterministic. `ProviderError` is not `Clone`, so the outcome is rebuilt
/// fresh on each call rather than stored.
struct FakeProvider {
    mode: FakeMode,
    rate_card: RateCard,
}

impl FakeProvider {
    fn complete_ok_with_actual_model() -> Self {
        Self {
            mode: FakeMode::CompleteOk {
                raw_response_model: Some("claude-opus-4-8-actual"),
                cost_cents: 42,
            },
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }

    fn complete_ok_without_actual_model() -> Self {
        Self {
            mode: FakeMode::CompleteOk {
                raw_response_model: None,
                cost_cents: 43,
            },
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }

    fn complete_err() -> Self {
        Self {
            mode: FakeMode::CompleteErr,
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }

    fn stream_ok() -> Self {
        Self {
            mode: FakeMode::StreamOk,
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }

    fn stream_start_err() -> Self {
        Self {
            mode: FakeMode::StreamStartErr,
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }

    fn stream_mid_err() -> Self {
        Self {
            mode: FakeMode::StreamMidErr,
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }
}

#[async_trait]
impl Provider for FakeProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        match self.mode {
            FakeMode::CompleteErr => Err(ProviderError::Unauthorized),
            FakeMode::CompleteOk {
                raw_response_model,
                cost_cents,
            } => {
                let raw_provider_response = raw_response_model
                    .map(|model| serde_json::json!({ "id": "msg_fake", "model": model }));
                Ok(CompletionResponse {
                    content: "hello from the fake provider".to_string(),
                    finish_reason: FinishReason::Stop,
                    usage: Usage {
                        tokens_in: 11,
                        tokens_out: 7,
                        cost_cents: Some(cost_cents),
                    },
                    cost: CostTuple {
                        tokens_in: 11,
                        tokens_out: 7,
                        cents: cost_cents,
                        wall_ms: 123,
                        attention_score: 0.0,
                    },
                    raw_provider_response,
                })
            }
            FakeMode::StreamOk | FakeMode::StreamStartErr | FakeMode::StreamMidErr => {
                Ok(CompletionResponse {
                    content: "fallback complete".to_string(),
                    finish_reason: FinishReason::Stop,
                    usage: Usage {
                        tokens_in: 1,
                        tokens_out: 1,
                        cost_cents: Some(1),
                    },
                    cost: CostTuple {
                        tokens_in: 1,
                        tokens_out: 1,
                        cents: 1,
                        wall_ms: 1,
                        attention_score: 0.0,
                    },
                    raw_provider_response: None,
                })
            }
        }
    }

    async fn stream(&self, _req: CompletionRequest) -> Result<ProviderStream, ProviderError> {
        match self.mode {
            FakeMode::StreamStartErr => Err(ProviderError::Unauthorized),
            FakeMode::StreamOk | FakeMode::StreamMidErr => Ok(Box::pin(FakeStream {
                step: 0,
                fail_mid_stream: matches!(self.mode, FakeMode::StreamMidErr),
            })),
            FakeMode::CompleteOk { .. } | FakeMode::CompleteErr => Err(
                ProviderError::InvalidRequest("stream not scripted".to_string()),
            ),
        }
    }

    fn id(&self) -> ProviderId {
        ProviderId("anthropic".to_string())
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// A fake provider stream that emits a child span while it is being polled. The
/// instrumentation wrapper should make that child span nest under
/// `provider.send` while recording final usage/finish/error on `provider.send`.
struct FakeStream {
    step: u8,
    fail_mid_stream: bool,
}

impl Stream for FakeStream {
    type Item = Result<StreamEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        tracing::info_span!("fake.inner.poll").in_scope(|| {});
        let item = match self.step {
            0 => Some(Ok(StreamEvent::ContentDelta("he".to_string()))),
            1 if self.fail_mid_stream => Some(Err(ProviderError::Upstream(
                "upstream stream interrupted".to_string(),
            ))),
            1 => Some(Ok(StreamEvent::ContentDelta("llo".to_string()))),
            2 => Some(Ok(StreamEvent::Usage(Usage {
                tokens_in: 17,
                tokens_out: 19,
                cost_cents: Some(99),
            }))),
            3 => Some(Ok(StreamEvent::Finish(FinishReason::MaxTokens))),
            _ => None,
        };
        self.step = self.step.saturating_add(1);
        Poll::Ready(item)
    }
}

fn request() -> CompletionRequest {
    let mut req = CompletionRequest::new(
        vec![ChatMessage::user("hi")],
        ModelId::new("claude-opus-4-8"),
        256,
    );
    req.temperature = 0.5;
    req
}

/// Run `provider.complete` under the capture layer and return the captured
/// spans. Synchronous on purpose: a current-thread runtime drives the future on
/// *this* thread, so the thread-local subscriber installed by `with_default` is
/// active while the span is entered/exited.
fn capture_complete_spans(provider: Arc<dyn Provider>) -> Vec<CapturedSpan> {
    let layer = CaptureLayer::default();
    let spans = layer.spans.clone();
    let subscriber = tracing_subscriber::registry().with(layer);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime builds");
    with_default(subscriber, || {
        rt.block_on(async {
            let _ = provider.complete(request()).await;
        });
    });
    spans.lock().expect("span buffer lock").clone()
}

/// Run `provider.stream`, drain the stream, then return captured spans. Draining
/// the stream is required because the provider.send span must stay open until
/// stream close, when final usage/finish/error attributes have been recorded.
fn capture_stream_spans(provider: Arc<dyn Provider>) -> Vec<CapturedSpan> {
    let layer = CaptureLayer::default();
    let spans = layer.spans.clone();
    let subscriber = tracing_subscriber::registry().with(layer);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime builds");
    with_default(subscriber, || {
        rt.block_on(async {
            match provider.stream(request().streaming()).await {
                Ok(stream) => {
                    let _events = stream.collect::<Vec<_>>().await;
                }
                Err(_err) => {}
            }
        });
    });
    spans.lock().expect("span buffer lock").clone()
}

fn provider_send_span(spans: &[CapturedSpan]) -> CapturedSpan {
    spans
        .iter()
        .find(|s| s.name == "provider.send")
        .cloned()
        .expect("a `provider.send` span was emitted")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn complete_success_span_carries_actual_response_model_usage_and_cost() {
    let provider =
        InstrumentedProvider::wrap(Arc::new(FakeProvider::complete_ok_with_actual_model()));
    let spans = capture_complete_spans(provider);
    let span = provider_send_span(&spans);

    assert_eq!(
        span.fields.get("gen_ai.system").map(String::as_str),
        Some("anthropic")
    );
    assert_eq!(
        span.fields.get("gen_ai.operation.name").map(String::as_str),
        Some("chat")
    );
    assert_eq!(
        span.fields.get("gen_ai.request.model").map(String::as_str),
        Some("claude-opus-4-8")
    );
    assert_eq!(
        span.fields.get("gen_ai.response.model").map(String::as_str),
        Some("claude-opus-4-8-actual")
    );
    assert_eq!(
        span.fields
            .get("gen_ai.request.max_tokens")
            .map(String::as_str),
        Some("256")
    );
    assert_eq!(
        span.fields
            .get("gen_ai.usage.input_tokens")
            .map(String::as_str),
        Some("11")
    );
    assert_eq!(
        span.fields
            .get("gen_ai.usage.output_tokens")
            .map(String::as_str),
        Some("7")
    );
    assert_eq!(
        span.fields
            .get("gen_ai.usage.cost_cents")
            .map(String::as_str),
        Some("42")
    );
    assert_eq!(
        span.fields
            .get("gen_ai.response.finish_reasons")
            .map(String::as_str),
        Some("[\"stop\"]")
    );
    assert!(
        !span.fields.contains_key("error.type"),
        "no error.type on a successful call; got {:?}",
        span.fields
    );
}

#[test]
fn complete_success_span_falls_back_to_requested_model_when_response_model_missing() {
    let provider =
        InstrumentedProvider::wrap(Arc::new(FakeProvider::complete_ok_without_actual_model()));
    let spans = capture_complete_spans(provider);
    let span = provider_send_span(&spans);

    assert_eq!(
        span.fields.get("gen_ai.response.model").map(String::as_str),
        Some("claude-opus-4-8")
    );
    assert_eq!(
        span.fields
            .get("gen_ai.usage.cost_cents")
            .map(String::as_str),
        Some("43")
    );
}

#[test]
fn complete_error_span_carries_error_type_and_error_finish_reason() {
    let provider = InstrumentedProvider::wrap(Arc::new(FakeProvider::complete_err()));
    let spans = capture_complete_spans(provider);
    let span = provider_send_span(&spans);

    assert_eq!(
        span.fields.get("error.type").map(String::as_str),
        Some("Unauthorized")
    );
    assert_eq!(
        span.fields
            .get("gen_ai.response.finish_reasons")
            .map(String::as_str),
        Some("[\"error\"]")
    );
    assert_eq!(
        span.fields.get("gen_ai.system").map(String::as_str),
        Some("anthropic")
    );
    assert_eq!(
        span.fields.get("gen_ai.request.model").map(String::as_str),
        Some("claude-opus-4-8")
    );
    assert!(
        !span.fields.contains_key("gen_ai.usage.cost_cents"),
        "failed calls must not report a fabricated cost; got {:?}",
        span.fields
    );
}

#[test]
fn stream_success_span_records_final_usage_finish_model_cost_and_nested_poll_span() {
    let provider = InstrumentedProvider::wrap(Arc::new(FakeProvider::stream_ok()));
    let spans = capture_stream_spans(provider);
    let span = provider_send_span(&spans);

    assert_eq!(
        span.fields.get("gen_ai.system").map(String::as_str),
        Some("anthropic")
    );
    assert_eq!(
        span.fields.get("gen_ai.request.model").map(String::as_str),
        Some("claude-opus-4-8")
    );
    assert_eq!(
        span.fields.get("gen_ai.response.model").map(String::as_str),
        Some("claude-opus-4-8")
    );
    assert_eq!(
        span.fields
            .get("gen_ai.usage.input_tokens")
            .map(String::as_str),
        Some("17")
    );
    assert_eq!(
        span.fields
            .get("gen_ai.usage.output_tokens")
            .map(String::as_str),
        Some("19")
    );
    assert_eq!(
        span.fields
            .get("gen_ai.usage.cost_cents")
            .map(String::as_str),
        Some("99")
    );
    assert_eq!(
        span.fields
            .get("gen_ai.response.finish_reasons")
            .map(String::as_str),
        Some("[\"length\"]")
    );
    assert!(
        spans.iter().any(|s| {
            s.name == "fake.inner.poll" && s.parent_name.as_deref() == Some("provider.send")
        }),
        "inner stream spans should nest under provider.send; got {spans:?}"
    );
}

#[test]
fn stream_start_error_span_records_error_without_usage_or_cost() {
    let provider = InstrumentedProvider::wrap(Arc::new(FakeProvider::stream_start_err()));
    let spans = capture_stream_spans(provider);
    let span = provider_send_span(&spans);

    assert_eq!(
        span.fields.get("error.type").map(String::as_str),
        Some("Unauthorized")
    );
    assert_eq!(
        span.fields
            .get("gen_ai.response.finish_reasons")
            .map(String::as_str),
        Some("[\"error\"]")
    );
    assert!(
        !span.fields.contains_key("gen_ai.usage.input_tokens")
            && !span.fields.contains_key("gen_ai.usage.cost_cents"),
        "start errors must not fabricate usage/cost; got {:?}",
        span.fields
    );
}

#[test]
fn stream_mid_error_span_records_error_finish_reason_and_preserves_nested_parent() {
    let provider = InstrumentedProvider::wrap(Arc::new(FakeProvider::stream_mid_err()));
    let spans = capture_stream_spans(provider);
    let span = provider_send_span(&spans);

    assert_eq!(
        span.fields.get("error.type").map(String::as_str),
        Some("Upstream")
    );
    assert_eq!(
        span.fields
            .get("gen_ai.response.finish_reasons")
            .map(String::as_str),
        Some("[\"error\"]")
    );
    assert!(
        !span.fields.contains_key("gen_ai.usage.cost_cents"),
        "mid-stream errors before a usage event must not fabricate cost; got {:?}",
        span.fields
    );
    assert!(
        spans.iter().any(|s| {
            s.name == "fake.inner.poll" && s.parent_name.as_deref() == Some("provider.send")
        }),
        "inner stream spans should still nest under provider.send on errors; got {spans:?}"
    );
}
