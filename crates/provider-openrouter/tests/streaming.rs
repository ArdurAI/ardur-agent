//! §3.2b — wiremock round-trips against OpenRouter's OpenAI-compatible SSE
//! streaming endpoint.
//!
//! A `wiremock` server stands in for the streaming `POST /chat/completions`,
//! serving a `text/event-stream` body of `data: {…}\n\n` chunks (terminated by
//! `data: [DONE]`) so the chunk parsing, incremental tool-call assembly, usage
//! extraction, `[DONE]` termination, and drop-cancellation are all asserted
//! offline (CI has no API key). A final gated test hits the real endpoint.

use ardur_provider_openrouter::{OpenRouterChunk, OpenRouterConfig, OpenRouterProvider};
use ardur_provider_runtime::{ChatMessage, CompletionRequest, FinishReason, ModelId, Provider};
use futures::StreamExt;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a provider whose base URL points at `server`.
fn provider_for(server: &MockServer, model: &str) -> OpenRouterProvider {
    OpenRouterProvider::new(
        OpenRouterConfig::new("sk-test").base_url(server.uri()),
        ModelId::new(model),
    )
}

/// Frame a list of JSON payloads as an SSE `text/event-stream` body, each as a
/// `data: {…}\n\n` event, terminated by the `data: [DONE]\n\n` marker.
fn sse_body(payloads: &[&str]) -> String {
    let mut body = String::new();
    for p in payloads {
        body.push_str("data: ");
        body.push_str(p);
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

/// Mount an SSE response on `server` and collect the whole `stream_chat` feed.
async fn collect_stream(server: &MockServer, body: String) -> Vec<OpenRouterChunk> {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        // The streaming request opts into `stream: true` + usage on the final chunk.
        .and(body_partial_json(serde_json::json!({
            "stream": true,
            "stream_options": {"include_usage": true},
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/event-stream"),
        )
        .expect(1)
        .mount(server)
        .await;

    let provider = provider_for(server, "openai/gpt-4o");
    let req = CompletionRequest::new(
        vec![ChatMessage::user("ping")],
        ModelId::new("openai/gpt-4o"),
        64,
    );
    provider
        .stream_chat(req)
        .await
        .map(|r| r.expect("no stream error"))
        .collect()
        .await
}

#[tokio::test]
async fn parses_content_delta_chunk() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":"Hello"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":", world"}}]}"#,
    ]);
    let chunks = collect_stream(&server, body).await;
    let text: String = chunks
        .iter()
        .filter_map(|c| match c {
            OpenRouterChunk::Content(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello, world");
}

#[tokio::test]
async fn parses_tool_call_delta_assembly() {
    let server = MockServer::start().await;
    // A tool call streamed across three chunks: id+name first, then two
    // `arguments` fragments, then the terminal `tool_calls` finish.
    let body = sse_body(&[
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":""}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"loc"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ation\":\"SF\"}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]);
    let chunks = collect_stream(&server, body).await;

    // The terminal Done event carries the fully reassembled tool call.
    let done = chunks
        .iter()
        .find_map(|c| match c {
            OpenRouterChunk::Done(fr) => Some(fr),
            _ => None,
        })
        .expect("a Done event");
    match done {
        FinishReason::ToolUse(calls) => {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].id, "call_1");
            assert_eq!(calls[0].name, "get_weather");
            assert_eq!(calls[0].arguments, serde_json::json!({"location": "SF"}));
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }

    // And the raw incremental fragments were surfaced along the way.
    let frag_count = chunks
        .iter()
        .filter(|c| matches!(c, OpenRouterChunk::ToolCall(_)))
        .count();
    assert_eq!(frag_count, 3, "one ToolCall delta per streamed fragment");
}

#[tokio::test]
async fn parses_finish_reason_tool_calls() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"ping","arguments":"{}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]);
    let chunks = collect_stream(&server, body).await;
    let done = chunks.last().expect("at least one event");
    assert!(
        matches!(done, OpenRouterChunk::Done(FinishReason::ToolUse(calls)) if calls.len() == 1),
        "expected Done(ToolUse), got {done:?}",
    );
}

#[tokio::test]
async fn parses_finish_reason_stop() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        r#"{"choices":[{"index":0,"delta":{"content":"done"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    ]);
    let chunks = collect_stream(&server, body).await;
    assert!(
        chunks
            .iter()
            .any(|c| matches!(c, OpenRouterChunk::Done(FinishReason::Stop))),
        "expected a Done(Stop) event in {chunks:?}",
    );
}

#[tokio::test]
async fn parses_usage_on_final_chunk() {
    let server = MockServer::start().await;
    // OpenAI emits the usage chunk last, with an empty `choices` array.
    let body = sse_body(&[
        r#"{"choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":5,"total_tokens":17}}"#,
    ]);
    let chunks = collect_stream(&server, body).await;
    let usage = chunks
        .iter()
        .find_map(|c| match c {
            OpenRouterChunk::Usage(u) => Some(u),
            _ => None,
        })
        .expect("a Usage event");
    assert_eq!(usage.tokens_in, 12);
    assert_eq!(usage.tokens_out, 5);
}

#[tokio::test]
async fn handles_done_marker() {
    let server = MockServer::start().await;
    // Two content chunks, then `[DONE]`, then trailing bytes that must never be
    // yielded — the stream terminates cleanly at the marker.
    let mut body = String::new();
    body.push_str("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"}}]}\n\n");
    body.push_str("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"b\"}}]}\n\n");
    body.push_str("data: [DONE]\n\n");
    body.push_str("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"NEVER\"}}]}\n\n");

    let chunks = collect_stream(&server, body).await;
    let text: String = chunks
        .iter()
        .filter_map(|c| match c {
            OpenRouterChunk::Content(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "ab", "nothing after [DONE] is yielded");
}

#[tokio::test]
async fn aborts_on_drop() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        r#"{"choices":[{"index":0,"delta":{"content":"first"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":"second"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    ]);
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let provider = provider_for(&server, "openai/gpt-4o");
    let req = CompletionRequest::new(
        vec![ChatMessage::user("ping")],
        ModelId::new("openai/gpt-4o"),
        64,
    );

    let mut stream = Box::pin(provider.stream_chat(req).await);
    // Pull one event, then drop the stream mid-flight: the underlying reqwest
    // body is dropped and the connection closed, with no panic.
    let first = stream.next().await.expect("first event").expect("no error");
    assert_eq!(first, OpenRouterChunk::Content("first".to_string()));
    drop(stream);
    // Reaching here without a panic is the assertion; give the drop a tick.
    tokio::task::yield_now().await;
}

#[tokio::test]
async fn non_streaming_path_still_works() {
    // Regression: `complete()` keeps its non-streaming `stream: false` body and
    // parses a normal JSON response, untouched by the streaming addition.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(body_partial_json(serde_json::json!({ "stream": false })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "gen-1",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "pong"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6, "cost": 0.001}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let provider = provider_for(&server, "openai/gpt-4o");
    let req = CompletionRequest::new(
        vec![ChatMessage::user("ping")],
        ModelId::new("openai/gpt-4o"),
        64,
    );
    let resp = provider.complete(req).await.expect("the call succeeds");
    assert_eq!(resp.content, "pong");
    assert!(matches!(resp.finish_reason, FinishReason::Stop));
}

/// Live smoke test against the real OpenRouter streaming endpoint.
///
/// Gated on `OPENROUTER_LIVE_STREAM_TEST=1` *and* `OPENROUTER_API_KEY`, so CI
/// (which sets neither) skips it. Asserts the stream yields some text and a
/// terminal finish reason.
#[tokio::test]
async fn live_stream_smoke() {
    if std::env::var("OPENROUTER_LIVE_STREAM_TEST").as_deref() != Ok("1") {
        eprintln!("skipping live stream test: set OPENROUTER_LIVE_STREAM_TEST=1 to run");
        return;
    }
    let model =
        std::env::var("OPENROUTER_LIVE_MODEL").unwrap_or_else(|_| "openai/gpt-4o-mini".to_string());
    let provider = match OpenRouterProvider::from_env(ModelId::new(&model)) {
        Ok(p) => p,
        Err(_) => {
            eprintln!("skipping live stream test: OPENROUTER_API_KEY unset");
            return;
        }
    };
    let req = CompletionRequest::new(
        vec![ChatMessage::user("Reply with exactly: pong")],
        ModelId::new(&model),
        32,
    );

    let mut stream = Box::pin(provider.stream_chat(req).await);
    let mut text = String::new();
    let mut saw_done = false;
    while let Some(item) = stream.next().await {
        match item.expect("no stream error") {
            OpenRouterChunk::Content(t) => text.push_str(&t),
            OpenRouterChunk::Done(_) => saw_done = true,
            _ => {}
        }
    }
    assert!(saw_done, "expected a terminal Done event");
    assert!(!text.is_empty(), "expected some streamed text, got empty");
}
