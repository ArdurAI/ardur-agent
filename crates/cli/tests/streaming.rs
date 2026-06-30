//! §2.1b — the CLI's progressive streaming display ([`ardur_cli::drive_turn`]).
//!
//! These drive the public render surface in-process against a configurable mock
//! [`Provider`]: a streaming-capable backend renders its
//! [`StreamEvent`](ardur_provider_runtime::StreamEvent) feed progressively, a
//! non-streaming backend (and the `--no-stream` gate) falls back to
//! `complete()`, and a mid-stream error keeps the partial output already shown.
//! Output is captured into a byte buffer through an unstyled (plain) theme so the
//! assertions match the no-ANSI rendering.

use ardur_cli::{RenderCtx, StreamOutcome, Theme, ThemeName, drive_turn};
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, ModelId, Provider, ProviderError,
    ProviderStream, RateCard, StreamEvent, Usage,
};
use ardur_runtime::{CostTuple, ProviderId, ToolCall};
use async_trait::async_trait;

/// A mock backend whose `stream()` replays a scripted event sequence and whose
/// `complete()` returns a fixed response — so a test can prove which path ran.
struct MockProvider {
    rate_card: RateCard,
    supports_streaming: bool,
    /// Content deltas the stream emits, in order.
    deltas: Vec<String>,
    /// `(id, name)` pairs emitted as `ToolCallStart` events after the deltas.
    tool_calls: Vec<(String, String)>,
    /// The usage ledger the stream reports (and the cost line is priced from).
    usage: Option<Usage>,
    /// When true, the stream yields an `Err` after its deltas instead of a
    /// terminal `Finish` — modelling a mid-stream failure.
    error_after_deltas: bool,
    /// The text `complete()` returns (the non-streaming fallback path).
    complete_content: String,
}

impl MockProvider {
    fn new() -> Self {
        Self {
            rate_card: RateCard::anthropic_2026_q2_v1(),
            supports_streaming: true,
            deltas: Vec::new(),
            tool_calls: Vec::new(),
            usage: None,
            error_after_deltas: false,
            complete_content: "complete-path-response".to_string(),
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let usage = self.usage.unwrap_or_default();
        Ok(CompletionResponse {
            content: self.complete_content.clone(),
            finish_reason: FinishReason::Stop,
            usage,
            cost: self.rate_card.price(usage),
            raw_provider_response: None,
        })
    }

    async fn stream(&self, _req: CompletionRequest) -> Result<ProviderStream, ProviderError> {
        let mut events: Vec<Result<StreamEvent, ProviderError>> = Vec::new();
        for delta in &self.deltas {
            events.push(Ok(StreamEvent::ContentDelta(delta.clone())));
        }
        for (id, name) in &self.tool_calls {
            events.push(Ok(StreamEvent::ToolCallStart(ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: serde_json::Value::Null,
            })));
        }
        if let Some(usage) = self.usage {
            events.push(Ok(StreamEvent::Usage(usage)));
        }
        if self.error_after_deltas {
            events.push(Err(ProviderError::Upstream("midstream boom".to_string())));
        } else {
            events.push(Ok(StreamEvent::Finish(FinishReason::Stop)));
        }
        Ok(Box::pin(futures::stream::iter(events)))
    }

    fn id(&self) -> ProviderId {
        ProviderId("mock".to_string())
    }

    fn supports_streaming(&self) -> bool {
        self.supports_streaming
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// A no-tools, no-tools streaming request against the mock's model.
fn request() -> CompletionRequest {
    CompletionRequest::new(Vec::new(), ModelId::new("mock-model"), 1024).streaming()
}

/// Drive `provider` through [`drive_turn`] with `stream_enabled`, capturing the
/// plain (no-color) output and the [`StreamOutcome`].
async fn run(provider: &MockProvider, stream_enabled: bool) -> (String, StreamOutcome) {
    let mut buf: Vec<u8> = Vec::new();
    // An unstyled theme at a fixed width keeps the captured output escape-free and
    // deterministic regardless of the test terminal.
    let theme = Theme::named(ThemeName::Night).plain();
    let ctx = RenderCtx::new(&theme, 80);
    let outcome = drive_turn(provider, request(), stream_enabled, &mut buf, &ctx)
        .await
        .expect("rendering to an in-memory buffer never fails");
    (String::from_utf8(buf).expect("utf-8 output"), outcome)
}

#[tokio::test]
async fn streaming_renders_content_deltas() {
    let mut provider = MockProvider::new();
    provider.deltas = vec![
        "Hello, ".to_string(),
        "stream".to_string(),
        "ing!".to_string(),
    ];

    let (output, outcome) = run(&provider, true).await;

    // All three deltas appear, concatenated in arrival order.
    assert!(
        output.contains("Hello, streaming!"),
        "streamed deltas should print concatenated, got: {output:?}"
    );
    assert_eq!(outcome.content, "Hello, streaming!");
    assert!(outcome.error.is_none(), "a clean stream has no error");
}

#[tokio::test]
async fn streaming_shows_tool_call_indicator() {
    let mut provider = MockProvider::new();
    provider.deltas = vec!["thinking".to_string()];
    provider.tool_calls = vec![("call-1".to_string(), "search_web".to_string())];

    let (output, outcome) = run(&provider, true).await;

    assert!(
        output.contains("tool · search_web"),
        "a ToolCallStart should render the tool box, got: {output:?}"
    );
    assert_eq!(outcome.tool_calls, vec!["search_web".to_string()]);
}

#[tokio::test]
async fn streaming_shows_usage_at_end() {
    let mut provider = MockProvider::new();
    provider.deltas = vec!["hi".to_string()];
    provider.usage = Some(Usage {
        tokens_in: 1000,
        tokens_out: 1000,
        cost_cents: None,
    });

    let (output, outcome) = run(&provider, true).await;

    // The dim end-of-turn cost line names the token split and the priced cost.
    // Under the Anthropic Q2-2026 card, 1k in + 1k out = 0.3 + 1.5 = ~2c -> $0.02.
    assert!(
        output.contains("1000 tokens in · 1000 out") && output.contains("$0.02"),
        "the usage/cost line should close the turn, got: {output:?}"
    );
    assert_eq!(
        outcome.usage,
        Some(Usage {
            tokens_in: 1000,
            tokens_out: 1000,
            cost_cents: None,
        })
    );
}

#[tokio::test]
async fn streaming_falls_back_when_not_supported() {
    let mut provider = MockProvider::new();
    provider.supports_streaming = false;
    // The stream() override (were it called) would emit this — proving it is NOT.
    provider.deltas = vec!["STREAMED".to_string()];

    // Even with streaming requested, an unsupported backend renders via complete().
    let (output, outcome) = run(&provider, true).await;

    assert!(
        output.contains("complete-path-response"),
        "a non-streaming backend should use complete(), got: {output:?}"
    );
    assert!(
        !output.contains("STREAMED"),
        "the stream() path must not run for an unsupported backend, got: {output:?}"
    );
    assert_eq!(outcome.content, "complete-path-response");
}

#[tokio::test]
async fn streaming_partial_on_error() {
    let mut provider = MockProvider::new();
    provider.deltas = vec!["partial output".to_string()];
    provider.error_after_deltas = true;

    let (output, outcome) = run(&provider, true).await;

    // The partial output rendered before the failure is preserved...
    assert!(
        output.contains("partial output"),
        "partial output should survive a mid-stream error, got: {output:?}"
    );
    // ...and the error is reported rather than the process crashing.
    assert!(
        output.contains("error:") && output.contains("midstream boom"),
        "the mid-stream error should be printed, got: {output:?}"
    );
    assert_eq!(outcome.content, "partial output");
    assert!(outcome.error.is_some(), "the outcome records the error");
}

#[tokio::test]
async fn no_stream_flag_uses_complete() {
    let mut provider = MockProvider::new();
    // The backend *can* stream, but the `--no-stream` gate (stream_enabled=false)
    // forces the complete() path.
    provider.deltas = vec!["STREAMED".to_string()];

    let (output, outcome) = run(&provider, false).await;

    assert!(
        output.contains("complete-path-response"),
        "--no-stream should render via complete(), got: {output:?}"
    );
    assert!(
        !output.contains("STREAMED"),
        "--no-stream must not consume the stream, got: {output:?}"
    );
    assert_eq!(outcome.content, "complete-path-response");
}

/// A serde round-trip of the shared cost type the rate card produces — the E2E
/// rule's public-surface check for this PR's value types.
#[test]
fn cost_tuple_round_trips() {
    let card = RateCard::anthropic_2026_q2_v1();
    let cost = card.price(Usage {
        tokens_in: 1000,
        tokens_out: 1000,
        cost_cents: None,
    });
    let json = serde_json::to_string(&cost).expect("serialize CostTuple");
    let back: CostTuple = serde_json::from_str(&json).expect("deserialize CostTuple");
    assert_eq!(cost, back);
}
