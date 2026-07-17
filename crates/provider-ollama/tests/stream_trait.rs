//! §3.X — the shared [`Provider::stream`] surface over Ollama's NDJSON feed.
//!
//! `Provider::stream` is the uniform streaming method every backend presents;
//! Ollama overrides the trait default to adapt its `OllamaChatChunk` NDJSON feed
//! into shared [`StreamEvent`]s. These tests drive that trait method against a
//! `wiremock` server serving NDJSON fixtures (no daemon, no key) and assert the
//! adapted event sequence: a [`StreamEvent::ContentDelta`] per token chunk, then
//! a terminal [`StreamEvent::Usage`] and [`StreamEvent::Finish`] folded from the
//! `done` chunk. (Ollama advertises no tools, so no `ToolCall*` events arise.)

use ardur_provider_ollama::{OllamaConfig, OllamaProvider};
use ardur_provider_runtime::{
    ChatMessage, CompletionRequest, FinishReason, ModelId, Provider, StreamEvent,
};
use futures::StreamExt;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A local provider (no auth) whose base URL points at `server`.
fn local_provider(server: &MockServer, model: &str) -> OllamaProvider {
    OllamaProvider::new(
        OllamaConfig::new()
            .base_url(server.uri())
            .default_model(model),
    )
}

/// A short chat request against `model`.
fn chat_req(model: &str) -> CompletionRequest {
    CompletionRequest::new(vec![ChatMessage::user("ping")], ModelId::new(model), 64)
}

/// Mount a `POST /api/chat` that asserts `stream: true` rode out and returns
/// `body` as a raw NDJSON response.
async fn mount_chat_ndjson(server: &MockServer, body: &'static str) {
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .and(body_partial_json(serde_json::json!({"stream": true})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/x-ndjson")
                .set_body_string(body),
        )
        .expect(1)
        .mount(server)
        .await;
}

/// Collect the whole shared-event feed from `Provider::stream`.
async fn collect_events(server: &MockServer, model: &str) -> Vec<StreamEvent> {
    let provider = local_provider(server, model);
    let stream = provider
        .stream(chat_req(model))
        .await
        .expect("the streaming handshake succeeds");
    stream.map(|r| r.expect("each event is Ok")).collect().await
}

/// A canonical three-token `/api/chat` NDJSON stream: two token chunks then a
/// terminal `done` chunk carrying the run's token counts.
const CHAT_NDJSON: &str = concat!(
    r#"{"model":"llama3.2","message":{"role":"assistant","content":"Hello"},"done":false}"#,
    "\n",
    r#"{"model":"llama3.2","message":{"role":"assistant","content":" world"},"done":false}"#,
    "\n",
    r#"{"model":"llama3.2","message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":11,"eval_count":2}"#,
    "\n",
);

#[tokio::test]
async fn content_deltas_pass_through() {
    let server = MockServer::start().await;
    mount_chat_ndjson(&server, CHAT_NDJSON).await;

    let events = collect_events(&server, "llama3.2").await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ContentDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello world");
    // The empty token on the terminal chunk produces no ContentDelta.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, StreamEvent::ContentDelta(_)))
            .count(),
        2,
    );
}

#[tokio::test]
async fn usage_and_finish_close_the_stream() {
    let server = MockServer::start().await;
    mount_chat_ndjson(&server, CHAT_NDJSON).await;

    let events = collect_events(&server, "llama3.2").await;

    // The last two events are Usage (folded token counts) then the terminal
    // Finish — the usage-then-finish order the receipt is minted from.
    let usage = events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Usage(u) => Some(*u),
            _ => None,
        })
        .expect("a Usage event");
    assert_eq!(usage.tokens_in, 11);
    assert_eq!(usage.tokens_out, 2);

    match events.last().expect("a terminal event") {
        StreamEvent::Finish(FinishReason::Stop) => {}
        other => panic!("expected Finish(Stop) last, got {other:?}"),
    }
    // Usage precedes Finish.
    let usage_pos = events
        .iter()
        .position(|e| matches!(e, StreamEvent::Usage(_)))
        .unwrap();
    let finish_pos = events
        .iter()
        .position(|e| matches!(e, StreamEvent::Finish(_)))
        .unwrap();
    assert!(usage_pos < finish_pos, "Usage is emitted before Finish");
}

#[tokio::test]
async fn finish_reason_length_propagates() {
    let server = MockServer::start().await;
    // A run cut off by the token cap reports `done_reason: "length"`.
    let body = concat!(
        r#"{"message":{"content":"trunc"},"done":false}"#,
        "\n",
        r#"{"message":{"content":""},"done":true,"done_reason":"length","prompt_eval_count":4,"eval_count":8}"#,
        "\n",
    );
    mount_chat_ndjson(&server, body).await;

    let events = collect_events(&server, "llama3.2").await;
    assert!(
        matches!(
            events.last(),
            Some(StreamEvent::Finish(FinishReason::MaxTokens))
        ),
        "expected Finish(MaxTokens), got {:?}",
        events.last(),
    );
}

#[tokio::test]
async fn no_tool_call_events_are_produced() {
    // Ollama advertises no tools, so the adapted stream is content + usage +
    // finish only — never a ToolCallStart / ToolCallDelta.
    let server = MockServer::start().await;
    mount_chat_ndjson(&server, CHAT_NDJSON).await;

    let events = collect_events(&server, "llama3.2").await;
    assert!(
        !events.iter().any(|e| matches!(
            e,
            StreamEvent::ToolCallStart(_) | StreamEvent::ToolCallDelta { .. }
        )),
        "no tool-call events from Ollama",
    );
}

/// Gated live test: requires `OLLAMA_LIVE_STREAM_TEST=1` and a running local
/// Ollama with the model pulled. Skipped (passes trivially) otherwise.
#[tokio::test]
async fn live_stream_trait_hits_real_endpoint() {
    if std::env::var("OLLAMA_LIVE_STREAM_TEST").as_deref() != Ok("1") {
        eprintln!("skipping: set OLLAMA_LIVE_STREAM_TEST=1 with a running Ollama to enable");
        return;
    }
    let model = std::env::var("OLLAMA_LIVE_MODEL").unwrap_or_else(|_| "llama3.2".to_string());
    let provider = OllamaProvider::from_env();
    let stream = provider
        .stream(CompletionRequest::new(
            vec![ChatMessage::user("Say the single word: pong")],
            ModelId::new(&model),
            64,
        ))
        .await
        .expect("the live streaming handshake succeeds");

    let events: Vec<_> = stream
        .map(|r| r.expect("each live event parses"))
        .collect()
        .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::ContentDelta(t) if !t.is_empty())),
        "the live stream yields at least one non-empty content delta",
    );
    let usage = events
        .iter()
        .find_map(|e| match e {
            StreamEvent::Usage(u) => Some(*u),
            _ => None,
        })
        .expect("a Usage event from the live stream");
    assert!(usage.tokens_out > 0, "the live run reports output tokens");
    assert!(
        matches!(events.last(), Some(StreamEvent::Finish(_))),
        "the live stream ends on a Finish event",
    );
}
