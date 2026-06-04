//! Scenario — `otel_genai_spans`.
//!
//! Proves the OpenTelemetry GenAI instrumentation end-to-end through the *fused*
//! substrate: a turn driven through [`FusedRuntime::submit`] — with its provider
//! wrapped in [`InstrumentedProvider`] — emits a `provider.send` span carrying
//! the `gen_ai.*` semantic-convention attributes, nested under the caller's
//! top-level `fused.submit` span.
//!
//! No OTLP collector is involved: an in-memory `tracing-subscriber` layer
//! captures the spans the OTel bridge would otherwise export, and the test
//! asserts both the span hierarchy and the GenAI attribute set directly. This is
//! the e2e half of the unit/integration coverage in
//! `crates/provider-runtime` (per the §E E2E-coverage rule).
//!
//! [`FusedRuntime::submit`]: ardur_fused_runtime::FusedRuntime::submit
//! [`InstrumentedProvider`]: ardur_provider_runtime::InstrumentedProvider

mod support;
use support::EchoProvider;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ardur_e2e_tests::fixtures;
use ardur_provider_runtime::InstrumentedProvider;
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, SessionId, SubmitRequest};
use tracing::Instrument;
use tracing::field::{Field, Visit};
use tracing::subscriber::with_default;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

// ---------------------------------------------------------------------------
// In-memory span capture (records name, parent name, and flattened fields)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct CapturedSpan {
    name: String,
    parent: Option<String>,
    fields: HashMap<String, String>,
}

#[derive(Clone, Default)]
struct CaptureLayer {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
}

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
        let parent = span.parent().map(|p| p.name().to_string());
        span.extensions_mut().insert(CapturedSpan {
            name: span.name().to_string(),
            parent,
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
// Scenario
// ---------------------------------------------------------------------------

/// A `SubmitRequest` carrying one user message under the deterministic, valid
/// cap-token the fused-runtime fixtures provision.
fn user_request(text: &str) -> SubmitRequest {
    SubmitRequest {
        messages: vec![ChatMessage::user(text)],
        cap_token: CapTokenRef(fixtures::dev_valid_cap_token()),
        session_id: SessionId::new(),
        requested_provider: None,
    }
}

/// Drive one fused turn through an `InstrumentedProvider`-wrapped stub, under a
/// top-level `fused.submit` span, and assert the emitted span tree.
#[test]
fn fused_turn_emits_nested_provider_send_span_with_genai_attrs() {
    let layer = CaptureLayer::default();
    let spans = layer.spans.clone();
    let subscriber = tracing_subscriber::registry().with(layer);

    // A current-thread runtime drives the future on *this* thread, so the
    // thread-local subscriber installed by `with_default` is active throughout
    // the turn and the nested `provider.send` span routes to our capture layer.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime builds");

    with_default(subscriber, || {
        rt.block_on(async {
            let provider = InstrumentedProvider::wrap(Arc::new(EchoProvider::new()));
            let runtime = fixtures::fused_builder(provider)
                .build()
                .expect("the fused runtime wires");

            // The top-level span the caller opens around the turn; the provider
            // dispatch nests under it.
            let span = tracing::info_span!("fused.submit");
            let result = runtime
                .submit(user_request("trace me"))
                .instrument(span)
                .await
                .expect("the turn completes");
            assert_eq!(
                result.response.content, "trace me",
                "the echo stub returns the prompt, proving the provider ran"
            );
        });
    });

    let spans = spans.lock().unwrap();

    // The top-level span is present.
    assert!(
        spans.iter().any(|s| s.name == "fused.submit"),
        "the caller's `fused.submit` span was captured; saw {:?}",
        spans.iter().map(|s| &s.name).collect::<Vec<_>>()
    );

    // The provider span is present, nested under `fused.submit`.
    let send = spans
        .iter()
        .find(|s| s.name == "provider.send")
        .expect("a `provider.send` span was emitted inside the turn");
    assert_eq!(
        send.parent.as_deref(),
        Some("fused.submit"),
        "the `provider.send` span nests under `fused.submit`"
    );

    // It carries the GenAI semconv attributes for the echo stub.
    assert_eq!(
        send.fields.get("gen_ai.system").map(String::as_str),
        Some("echo")
    );
    assert_eq!(
        send.fields.get("gen_ai.operation.name").map(String::as_str),
        Some("chat")
    );
    assert_eq!(
        send.fields.get("gen_ai.request.model").map(String::as_str),
        Some(fixtures::TEST_MODEL)
    );
    assert_eq!(
        send.fields
            .get("gen_ai.usage.input_tokens")
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(
        send.fields
            .get("gen_ai.usage.output_tokens")
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(
        send.fields
            .get("gen_ai.response.finish_reasons")
            .map(String::as_str),
        Some("[\"stop\"]")
    );
}
