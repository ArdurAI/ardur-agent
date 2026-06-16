//! Integration test for the OpenTelemetry GenAI span emission.
//!
//! [`InstrumentedProvider`] wraps a fake provider; an in-memory
//! `tracing-subscriber` layer captures the `provider.send` span it opens, and we
//! assert the span carries the GenAI semantic-convention attributes for both the
//! success and the error path. No OTLP collector is involved — the `tracing`
//! field values the OTel bridge would forward are asserted directly off the
//! captured span.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, InstrumentedProvider, ModelId, Provider,
    ProviderError, RateCard, Usage,
};
use ardur_runtime::{ChatMessage, CostTuple, ProviderId};
use async_trait::async_trait;
use tracing::field::{Field, Visit};
use tracing::subscriber::with_default;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

// ---------------------------------------------------------------------------
// In-memory span capture
// ---------------------------------------------------------------------------

/// One captured span: its name and the flattened field map (values stringified
/// the way the OTel bridge would serialize them).
#[derive(Clone, Debug, Default)]
struct CapturedSpan {
    name: String,
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
        span.extensions_mut().insert(CapturedSpan {
            name: span.name().to_string(),
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
            self.spans.lock().unwrap().push(captured);
        }
    }
}

// ---------------------------------------------------------------------------
// Fake provider that records its call and returns a scripted result
// ---------------------------------------------------------------------------

/// A provider that returns a fixed success or a fixed error, so the span's
/// response/error attributes are deterministic. `ProviderError` is not `Clone`,
/// so the outcome is rebuilt fresh on each call rather than stored.
struct FakeProvider {
    fail: bool,
    rate_card: RateCard,
}

impl FakeProvider {
    fn ok() -> Self {
        Self {
            fail: false,
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }

    fn err() -> Self {
        Self {
            fail: true,
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }
}

#[async_trait]
impl Provider for FakeProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        if self.fail {
            return Err(ProviderError::Unauthorized);
        }
        Ok(CompletionResponse {
            content: "hello from the fake provider".to_string(),
            finish_reason: FinishReason::Stop,
            usage: Usage {
                tokens_in: 11,
                tokens_out: 7,
            
            ..Default::default()
        },
            cost: CostTuple::default(),
            raw_provider_response: None,
        })
    }
    fn id(&self) -> ProviderId {
        ProviderId("anthropic".to_string())
    }
    fn supports_streaming(&self) -> bool {
        false
    }
    fn rate_card(&self) -> &RateCard {
        &self.rate_card
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
/// `provider.send` span. Synchronous on purpose: a current-thread runtime drives
/// the future on *this* thread, so the thread-local subscriber installed by
/// `with_default` is active while the span is entered/exited.
fn capture_send_span(provider: Arc<dyn Provider>) -> CapturedSpan {
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
    let spans = spans.lock().unwrap();
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
fn success_span_carries_genai_request_and_usage_attributes() {
    let provider = InstrumentedProvider::wrap(Arc::new(FakeProvider::ok()));
    let span = capture_send_span(provider);

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
        span.fields
            .get("gen_ai.request.max_tokens")
            .map(String::as_str),
        Some("256")
    );
    // The four attributes the task explicitly requires be present.
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
            .get("gen_ai.response.finish_reasons")
            .map(String::as_str),
        Some("[\"stop\"]")
    );
    // The error attribute is absent on the happy path.
    assert!(
        !span.fields.contains_key("error.type"),
        "no error.type on a successful call; got {:?}",
        span.fields
    );
}

#[test]
fn error_span_carries_error_type_and_error_finish_reason() {
    let provider = InstrumentedProvider::wrap(Arc::new(FakeProvider::err()));
    let span = capture_send_span(provider);

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
    // The request-side attributes are still recorded on the error path.
    assert_eq!(
        span.fields.get("gen_ai.system").map(String::as_str),
        Some("anthropic")
    );
    assert_eq!(
        span.fields.get("gen_ai.request.model").map(String::as_str),
        Some("claude-opus-4-8")
    );
}
