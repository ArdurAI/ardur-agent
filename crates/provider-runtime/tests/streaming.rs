//! §3.1b — Anthropic SSE streaming, parsed offline against `wiremock`.
//!
//! A `wiremock` server stands in for the Messages API and serves a canned
//! `text/event-stream` body, so the whole server-sent-event protocol —
//! `message_start`, `content_block_*`, `message_delta`, `message_stop` — is
//! decoded into [`StreamEvent`]s without ever touching the network or needing an
//! API key (CI has none). The final test pins the non-streaming `complete()`
//! path as a regression, and a key-gated test (`ANTHROPIC_LIVE_STREAM_TEST=1`)
//! hits the real streaming endpoint.

use ardur_provider_runtime::{
    AnthropicProvider, ChatMessage, CompletionRequest, FinishReason, ModelId, Provider,
    ProviderError, StreamEvent,
};
use futures::StreamExt;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// One SSE frame: `event: <name>\ndata: <json>\n\n`.
fn frame(event: &str, data: serde_json::Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// Build a live provider whose Messages-API endpoint points at `server`.
fn provider_for(server: &MockServer) -> AnthropicProvider {
    AnthropicProvider::new("sk-test", ModelId::new("claude-opus-4-8"))
        .with_base_url(format!("{}/v1/messages", server.uri()))
}

/// Mount a streaming `POST /v1/messages` that asserts `stream: true` is on the
/// wire and replies with `body` as an event stream.
async fn mount_sse(server: &MockServer, body: String) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-test"))
        .and(body_partial_json(serde_json::json!({ "stream": true })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .expect(1)
        .mount(server)
        .await;
}

/// Drain a provider's stream into a `Vec` of events, failing on any error item.
async fn collect(provider: &AnthropicProvider, req: CompletionRequest) -> Vec<StreamEvent> {
    let stream = provider.stream(req).await.expect("stream opens");
    stream
        .map(|item| item.expect("no error item in a well-formed stream"))
        .collect()
        .await
}

fn req() -> CompletionRequest {
    CompletionRequest::new(
        vec![ChatMessage::user("ping")],
        ModelId::new("claude-opus-4-8"),
        64,
    )
    .streaming()
}

#[tokio::test]
async fn parses_message_start_event() {
    let server = MockServer::start().await;
    let body = frame(
        "message_start",
        serde_json::json!({
            "type": "message_start",
            "message": { "usage": { "input_tokens": 17, "output_tokens": 0 } }
        }),
    ) + &frame(
        "message_stop",
        serde_json::json!({ "type": "message_stop" }),
    );
    mount_sse(&server, body).await;

    let events = collect(&provider_for(&server), req()).await;

    // The first event the stream surfaces is the input-token ledger from
    // `message_start`.
    assert_eq!(
        events.first(),
        Some(&StreamEvent::Usage(ardur_usage(17, 0))),
        "message_start should seed the input-token usage; got {events:?}"
    );
}

#[tokio::test]
async fn parses_content_block_delta_text() {
    let server = MockServer::start().await;
    let body = frame(
        "message_start",
        serde_json::json!({"message": {"usage": {"input_tokens": 3, "output_tokens": 0}}}),
    ) + &frame(
        "content_block_delta",
        serde_json::json!({"index": 0, "delta": {"type": "text_delta", "text": "Hel"}}),
    ) + &frame(
        "content_block_delta",
        serde_json::json!({"index": 0, "delta": {"type": "text_delta", "text": "lo"}}),
    ) + &frame("message_stop", serde_json::json!({}));
    mount_sse(&server, body).await;

    let events = collect(&provider_for(&server), req()).await;

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ContentDelta(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text, "Hello",
        "text deltas concatenate in order; got {events:?}"
    );
}

#[tokio::test]
async fn parses_content_block_start_tool_use() {
    let server = MockServer::start().await;
    let body = frame(
        "content_block_start",
        serde_json::json!({
            "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "get_weather"}
        }),
    ) + &frame("message_stop", serde_json::json!({}));
    mount_sse(&server, body).await;

    let events = collect(&provider_for(&server), req()).await;

    let start = events.iter().find_map(|e| match e {
        StreamEvent::ToolCallStart(call) => Some(call),
        _ => None,
    });
    let call = start.expect("a tool_use block start yields ToolCallStart");
    assert_eq!(call.id, "toolu_1");
    assert_eq!(call.name, "get_weather");
    // Arguments are not known yet at start — they stream as deltas.
    assert_eq!(call.arguments, serde_json::Value::Null);
}

#[tokio::test]
async fn parses_input_json_delta_for_tool_use() {
    let server = MockServer::start().await;
    let body = frame(
        "content_block_start",
        serde_json::json!({
            "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_9", "name": "search"}
        }),
    ) + &frame(
        "content_block_delta",
        serde_json::json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"q\":"}}),
    ) + &frame(
        "content_block_delta",
        serde_json::json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": "\"rust\"}"}}),
    ) + &frame("content_block_stop", serde_json::json!({"index": 0}))
        + &frame(
            "message_delta",
            serde_json::json!({"delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 12}}),
        )
        + &frame("message_stop", serde_json::json!({}));
    mount_sse(&server, body).await;

    let events = collect(&provider_for(&server), req()).await;

    // The argument fragments arrive keyed to the call id...
    let fragments: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallDelta { id, delta } if id == "toolu_9" => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(fragments, "{\"q\":\"rust\"}");

    // ...and the terminal finish carries the fully-assembled call.
    let finish = events.iter().find_map(|e| match e {
        StreamEvent::Finish(reason) => Some(reason),
        _ => None,
    });
    match finish.expect("a Finish event terminates the stream") {
        FinishReason::ToolUse(calls) => {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].id, "toolu_9");
            assert_eq!(calls[0].arguments, serde_json::json!({"q": "rust"}));
        }
        other => panic!("expected ToolUse finish, got {other:?}"),
    }
}

#[tokio::test]
async fn parses_message_delta_stop_reason() {
    let server = MockServer::start().await;
    let body = frame(
        "message_start",
        serde_json::json!({"message": {"usage": {"input_tokens": 5, "output_tokens": 0}}}),
    ) + &frame(
        "content_block_delta",
        serde_json::json!({"index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
    ) + &frame(
        "message_delta",
        serde_json::json!({"delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 64}}),
    ) + &frame("message_stop", serde_json::json!({}));
    mount_sse(&server, body).await;

    let events = collect(&provider_for(&server), req()).await;

    let finish = events.iter().find_map(|e| match e {
        StreamEvent::Finish(reason) => Some(reason),
        _ => None,
    });
    assert!(
        matches!(finish, Some(FinishReason::MaxTokens)),
        "message_delta stop_reason should drive the finish reason; got {events:?}"
    );
}

#[tokio::test]
async fn parses_message_stop_usage() {
    let server = MockServer::start().await;
    let body = frame(
        "message_start",
        serde_json::json!({"message": {"usage": {"input_tokens": 8, "output_tokens": 0}}}),
    ) + &frame(
        "message_delta",
        serde_json::json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 21}}),
    ) + &frame("message_stop", serde_json::json!({}));
    mount_sse(&server, body).await;

    let events = collect(&provider_for(&server), req()).await;

    // The terminal sequence is the final usage immediately followed by finish.
    let finish_idx = events
        .iter()
        .position(|e| matches!(e, StreamEvent::Finish(_)))
        .expect("a Finish event terminates the stream");
    assert!(finish_idx >= 1, "Finish is not the very first event");
    assert_eq!(
        events[finish_idx - 1],
        StreamEvent::Usage(ardur_usage(8, 21)),
        "message_stop emits the final usage just before finish; got {events:?}"
    );
    assert!(
        matches!(events[finish_idx], StreamEvent::Finish(FinishReason::Stop)),
        "end_turn maps to FinishReason::Stop"
    );
}

#[tokio::test]
async fn accumulates_usage_across_chunks() {
    let server = MockServer::start().await;
    // Input tokens land in `message_start`; output tokens land later in
    // `message_delta` — the final usage event must carry both.
    let body = frame(
        "message_start",
        serde_json::json!({"message": {"usage": {"input_tokens": 100, "output_tokens": 0}}}),
    ) + &frame(
        "content_block_delta",
        serde_json::json!({"index": 0, "delta": {"type": "text_delta", "text": "answer"}}),
    ) + &frame(
        "message_delta",
        serde_json::json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 250}}),
    ) + &frame("message_stop", serde_json::json!({}));
    mount_sse(&server, body).await;

    let events = collect(&provider_for(&server), req()).await;

    // The last usage event is the accumulated input+output ledger.
    let last_usage = events
        .iter()
        .rev()
        .find_map(|e| match e {
            StreamEvent::Usage(u) => Some(*u),
            _ => None,
        })
        .expect("at least one usage event");
    assert_eq!(last_usage.tokens_in, 100);
    assert_eq!(last_usage.tokens_out, 250);
}

#[tokio::test]
async fn non_streaming_path_still_works() {
    // Regression: the default `stream()` over a non-overriding provider (here the
    // stub) and the plain `complete()` both keep working unchanged.
    let provider = AnthropicProvider::stub(ModelId::new("claude-opus-4-8"));

    let resp = provider
        .complete(req())
        .await
        .expect("stub completion succeeds");
    assert_eq!(resp.content, "[anthropic stub]");
    assert!(matches!(resp.finish_reason, FinishReason::Stop));

    // The stub's stream replays that same response as events.
    let events = collect(&provider, req()).await;
    assert!(events.contains(&StreamEvent::ContentDelta("[anthropic stub]".to_string())));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::Finish(FinishReason::Stop))),
        "the stub stream terminates with a Finish; got {events:?}"
    );
}

#[tokio::test]
async fn non_2xx_status_is_an_error_before_streaming() {
    // An admission failure (e.g. 401) surfaces as the `Err` of `stream()`, not as
    // an in-stream error item — the same contract `complete()` honors.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": {"type": "authentication_error", "message": "bad key"}
        })))
        .mount(&server)
        .await;

    let result = provider_for(&server).stream(req()).await;
    assert!(
        matches!(result.err(), Some(ProviderError::Unauthorized)),
        "a 401 maps to ProviderError::Unauthorized before any stream is returned"
    );
}

/// §3.1b live streaming smoke test — gated on `ANTHROPIC_LIVE_STREAM_TEST=1` plus
/// a real `ANTHROPIC_API_KEY`. With either unset it is a no-op, so CI passes
/// without touching the network. With both present it streams one real
/// completion and asserts text and a final usage ledger flow through.
#[tokio::test]
async fn live_stream_round_trips() {
    if std::env::var("ANTHROPIC_LIVE_STREAM_TEST").as_deref() != Ok("1")
        || std::env::var("ANTHROPIC_API_KEY")
            .map(|k| k.is_empty())
            .unwrap_or(true)
    {
        eprintln!("skipped: set ANTHROPIC_LIVE_STREAM_TEST=1 and ANTHROPIC_API_KEY to run");
        return;
    }

    let model = ModelId::new("claude-opus-4-8");
    let provider = AnthropicProvider::from_env(model.clone()).expect("API key present → live");
    let mut request =
        CompletionRequest::new(vec![ChatMessage::user("Say only: ping")], model, 16).streaming();
    request.temperature = 0.0;

    let mut stream = provider.stream(request).await.expect("live stream opens");
    let mut text = String::new();
    let mut final_usage = None;
    let mut finished = false;
    while let Some(item) = stream.next().await {
        match item.expect("no error item on the live stream") {
            StreamEvent::ContentDelta(s) => text.push_str(&s),
            StreamEvent::Usage(u) => final_usage = Some(u),
            StreamEvent::Finish(_) => finished = true,
            _ => {}
        }
    }

    assert!(finished, "the live stream terminates with a Finish");
    assert!(
        text.to_lowercase().contains("ping"),
        "expected the model to echo 'ping', got: {text:?}"
    );
    let usage = final_usage.expect("a usage ledger flows through");
    assert!(usage.tokens_in > 0 && usage.tokens_out > 0);
}

/// Construct a [`Usage`](ardur_provider_runtime::Usage) inline. (`Usage` exposes
/// public fields but no positional constructor.)
fn ardur_usage(tokens_in: u32, tokens_out: u32) -> ardur_provider_runtime::Usage {
    ardur_provider_runtime::Usage {
        tokens_in,
        tokens_out,
        ..Default::default()
    }
}
