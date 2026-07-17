//! §3.X — the shared [`Provider::stream`] surface over an OpenAI-compatible SSE
//! feed.
//!
//! `Provider::stream` is the uniform streaming method every backend presents;
//! this provider overrides the trait default to adapt its OpenAI-compatible SSE
//! `OpenAiCompatChunk` feed into shared [`StreamEvent`]s — remapping OpenAI's
//! index-keyed tool-call deltas onto the shared id-keyed
//! [`StreamEvent::ToolCallStart`] / [`StreamEvent::ToolCallDelta`] events. These
//! tests drive that trait method against a `wiremock` server serving SSE
//! fixtures (no API key) and assert the adapted event sequence, including the
//! out-of-order id buffering the adapter performs.

use ardur_provider_openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use ardur_provider_runtime::{
    ChatMessage, CompletionRequest, FinishReason, ModelId, Provider, ProviderError, StreamEvent,
};
use futures::StreamExt;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a provider whose base URL points at `server`.
fn provider_for(server: &MockServer, model: &str) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(
        OpenAiCompatConfig::new("sk-test").base_url(server.uri()),
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

/// Mount an SSE response on `server` and collect the whole shared-event feed
/// `Provider::stream` yields.
async fn collect_events(server: &MockServer, body: String) -> Vec<StreamEvent> {
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
        .stream(req)
        .await
        .expect("the streaming handshake succeeds")
        .map(|r| r.expect("no stream error"))
        .collect()
        .await
}

/// Pull the assembled tool calls out of the terminal `Finish(ToolUse(..))` event.
fn finish_tool_calls(events: &[StreamEvent]) -> Vec<ardur_provider_runtime::ToolCall> {
    match events.last().expect("a terminal event") {
        StreamEvent::Finish(FinishReason::ToolUse(calls)) => calls.clone(),
        other => panic!("expected Finish(ToolUse) last, got {other:?}"),
    }
}

#[tokio::test]
async fn content_delta_passes_through() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":"Hello"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"content":", world"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    ]);
    let events = collect_events(&server, body).await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ContentDelta(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello, world");
}

#[tokio::test]
async fn tool_call_id_arrives_first() {
    let server = MockServer::start().await;
    // id + name in the first delta, arguments streamed in later fragments.
    let body = sse_body(&[
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":""}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"loc"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ation\":\"SF\"}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]);
    let events = collect_events(&server, body).await;

    // The start carries id + name; it precedes every delta for that call.
    let start_pos = events
        .iter()
        .position(|e| {
            matches!(e, StreamEvent::ToolCallStart(c) if c.id == "call_1" && c.name == "get_weather")
        })
        .expect("a ToolCallStart for call_1");
    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallDelta { id, delta } if id == "call_1" => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas.concat(), r#"{"location":"SF"}"#);
    let first_delta_pos = events
        .iter()
        .position(|e| matches!(e, StreamEvent::ToolCallDelta { .. }))
        .expect("a ToolCallDelta");
    assert!(start_pos < first_delta_pos, "start precedes its deltas");

    // The terminal Finish carries the fully assembled call.
    let calls = finish_tool_calls(&events);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_1");
    assert_eq!(calls[0].arguments, serde_json::json!({"location": "SF"}));
}

#[tokio::test]
async fn tool_call_id_arrives_late() {
    let server = MockServer::start().await;
    // Argument fragments stream BEFORE the fragment that reveals the id: the
    // adapter buffers them, then on the id fragment emits the start and flushes
    // the buffered deltas — all keyed by the (now known) id, never `id: ""`.
    let body = sse_body(&[
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"loc"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ation\":\"SF\"}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_late","type":"function","function":{"name":"get_weather"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]);
    let events = collect_events(&server, body).await;

    // No delta was emitted with an empty id (the buffering invariant).
    assert!(
        !events.iter().any(|e| matches!(
            e,
            StreamEvent::ToolCallDelta { id, .. } if id.is_empty()
        )),
        "no ToolCallDelta is emitted before its id is known",
    );

    // The start (with the late id) precedes the flushed deltas.
    let start_pos = events
        .iter()
        .position(|e| matches!(e, StreamEvent::ToolCallStart(c) if c.id == "call_late"))
        .expect("a ToolCallStart for call_late");
    let first_delta_pos = events
        .iter()
        .position(|e| matches!(e, StreamEvent::ToolCallDelta { .. }))
        .expect("a flushed ToolCallDelta");
    assert!(
        start_pos < first_delta_pos,
        "buffered deltas flush after start"
    );

    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallDelta { id, delta } if id == "call_late" => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas.concat(), r#"{"location":"SF"}"#);

    let calls = finish_tool_calls(&events);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_late");
    assert_eq!(calls[0].arguments, serde_json::json!({"location": "SF"}));
}

#[tokio::test]
async fn multi_index_tool_calls() {
    let server = MockServer::start().await;
    // Two parallel tool calls at index 0 and 1, their fragments interleaved.
    let body = sse_body(&[
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"alpha","arguments":""}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"beta","arguments":""}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"x\":1}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"y\":2}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
    ]);
    let events = collect_events(&server, body).await;

    // Two distinct starts, one per call id.
    let starts: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallStart(c) => Some((c.id.as_str(), c.name.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(starts, vec![("call_a", "alpha"), ("call_b", "beta")]);

    // Each call's argument delta is keyed to its own id.
    let a: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallDelta { id, delta } if id == "call_a" => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    let b: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallDelta { id, delta } if id == "call_b" => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(a, r#"{"x":1}"#);
    assert_eq!(b, r#"{"y":2}"#);

    let calls = finish_tool_calls(&events);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].arguments, serde_json::json!({"x": 1}));
    assert_eq!(calls[1].arguments, serde_json::json!({"y": 2}));
}

#[tokio::test]
async fn usage_at_end() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        r#"{"choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":5,"total_tokens":17,"cost":0.0234}}"#,
    ]);
    let events = collect_events(&server, body).await;
    let usage_pos = events
        .iter()
        .position(|e| matches!(e, StreamEvent::Usage(_)))
        .expect("a Usage event");
    let finish_pos = events
        .iter()
        .position(|e| matches!(e, StreamEvent::Finish(_)))
        .expect("a Finish event");
    assert!(
        usage_pos < finish_pos,
        "usage/cost must be emitted before terminal Finish so the runtime can price the turn"
    );

    let StreamEvent::Usage(usage) = events[usage_pos] else {
        unreachable!("usage_pos points at Usage")
    };
    assert_eq!(usage.tokens_in, 12);
    assert_eq!(usage.tokens_out, 5);
    // 0.0234 USD → 2.34¢ → 3¢, matching non-streaming ARD-495 behavior.
    assert_eq!(usage.cost_cents, Some(3));
    assert!(
        matches!(events.last(), Some(StreamEvent::Finish(FinishReason::Stop))),
        "Finish remains terminal after usage is surfaced"
    );
}

#[tokio::test]
async fn finish_reason_propagates() {
    let server = MockServer::start().await;
    let body = sse_body(&[
        r#"{"choices":[{"index":0,"delta":{"content":"done"}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    ]);
    let events = collect_events(&server, body).await;
    assert!(
        matches!(events.last(), Some(StreamEvent::Finish(FinishReason::Stop))),
        "expected a terminal Finish(Stop), got {:?}",
        events.last(),
    );
}

#[tokio::test]
async fn non_2xx_handshake_is_terminal_err() {
    // A 401 at the streaming handshake is the Err of the Result — surfaced before
    // any event yields, exactly as the trait contract promises.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": {"message": "bad key", "code": 401}
        })))
        .mount(&server)
        .await;

    let provider = provider_for(&server, "openai/gpt-4o");
    let req = CompletionRequest::new(
        vec![ChatMessage::user("ping")],
        ModelId::new("openai/gpt-4o"),
        64,
    );
    match provider.stream(req).await {
        Err(ProviderError::Unauthorized) => {}
        Ok(_) => panic!("a 401 handshake must not yield a stream"),
        Err(other) => panic!("expected Unauthorized, got {other:?}"),
    }
}

/// Live smoke test against the real OpenAI-compatible streaming endpoint, via the
/// shared `Provider::stream` surface. Gated on
/// `OPENAI_COMPAT_LIVE_STREAM_TEST=1`, `OPENAI_COMPAT_API_KEY`, and an explicit
/// `OPENAI_COMPAT_LIVE_MODEL`, so CI skips it and the test does not encode a
/// stale model default.
#[tokio::test]
async fn live_stream_trait_smoke() {
    if std::env::var("OPENAI_COMPAT_LIVE_STREAM_TEST").as_deref() != Ok("1") {
        eprintln!("skipping live stream test: set OPENAI_COMPAT_LIVE_STREAM_TEST=1 to run");
        return;
    }
    let Ok(model) = std::env::var("OPENAI_COMPAT_LIVE_MODEL") else {
        eprintln!("skipping live stream test: set OPENAI_COMPAT_LIVE_MODEL to run");
        return;
    };
    let provider = match OpenAiCompatProvider::from_env(ModelId::new(&model)) {
        Ok(p) => p,
        Err(_) => {
            eprintln!("skipping live stream test: OPENAI_COMPAT_API_KEY unset");
            return;
        }
    };
    let req = CompletionRequest::new(
        vec![ChatMessage::user("Reply with exactly: pong")],
        ModelId::new(&model),
        32,
    );

    let mut stream = provider
        .stream(req)
        .await
        .expect("the live streaming handshake succeeds");
    let mut text = String::new();
    let mut saw_finish = false;
    while let Some(item) = stream.next().await {
        match item.expect("no stream error") {
            StreamEvent::ContentDelta(t) => text.push_str(&t),
            StreamEvent::Finish(_) => saw_finish = true,
            _ => {}
        }
    }
    assert!(saw_finish, "expected a terminal Finish event");
    assert!(!text.is_empty(), "expected some streamed text, got empty");
}
