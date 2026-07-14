//! §3.4 Phase 2 — real incremental streaming for the Bedrock backend.
//!
//! `InvokeModelWithResponseStream` answers with an
//! `application/vnd.amazon.eventstream` binary frame feed (decoded by
//! [`crate::eventstream`]), not SSE. Each frame's `:message-type` header is
//! `"event"` (payload `{"bytes": "<base64 JSON>"}`, the base64 decoding to
//! one of Anthropic's native Messages-API streaming event types —
//! `message_start`/`content_block_start`/`content_block_delta`/
//! `content_block_stop`/`message_delta`/`message_stop`/`ping`, the same
//! events Anthropic's direct streaming API emits over SSE) or `"exception"`
//! (payload `{"message": "..."}`, terminating the stream as an error).
//!
//! Anthropic's content blocks are indexed by an integer `index`; a `tool_use`
//! block's `content_block_start` carries the real call `id`/`name`, and its
//! `input_json_delta` fragments are forwarded as
//! [`ToolCallDelta`](StreamEvent::ToolCallDelta)s keyed by that id — no
//! index→id remapping ambiguity the way OpenAI's index-only scheme has.
//!
//! Cancellation is by drop: dropping the returned stream drops the
//! underlying `reqwest` byte stream, closing the HTTP connection.

use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;

use ardur_provider_runtime::{FinishReason, ProviderError, StreamEvent, ToolCall, Usage};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::{Stream, StreamExt};

use crate::eventstream::{self, Message};
use crate::map_stop_reason;

/// Per-stream state the Anthropic-event decoder threads across frames:
/// cached input-token count (from `message_start`), open `tool_use` blocks
/// by content-block index (id, name, accumulated arguments string), and the
/// finalized tool calls collected as each block closes.
#[derive(Default)]
struct AnthropicEventState {
    input_tokens: u32,
    open_tool_blocks: BTreeMap<u64, (String, String, String)>, // index -> (id, name, args_buf)
    finished_calls: Vec<ToolCall>,
    pending_finish: Option<FinishReason>,
}

/// Decode one base64-decoded Anthropic streaming-event JSON payload into
/// zero-or-more shared [`StreamEvent`]s.
fn process_anthropic_event(
    json: &str,
    state: &mut AnthropicEventState,
    out: &mut VecDeque<Result<StreamEvent, ProviderError>>,
) {
    let value: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => {
            out.push_back(Err(ProviderError::Upstream(format!(
                "unexpected Bedrock stream event shape: {e}"
            ))));
            return;
        }
    };
    let event_type = value["type"].as_str().unwrap_or_default();

    match event_type {
        "message_start" => {
            state.input_tokens = value["message"]["usage"]["input_tokens"]
                .as_u64()
                .unwrap_or(0) as u32;
        }
        "content_block_start" => {
            let index = value["index"].as_u64().unwrap_or(0);
            let block = &value["content_block"];
            if block["type"].as_str() == Some("tool_use") {
                let id = block["id"].as_str().unwrap_or_default().to_string();
                let name = block["name"].as_str().unwrap_or_default().to_string();
                out.push_back(Ok(StreamEvent::ToolCallStart(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: serde_json::Value::Null,
                })));
                state
                    .open_tool_blocks
                    .insert(index, (id, name, String::new()));
            }
        }
        "content_block_delta" => {
            let index = value["index"].as_u64().unwrap_or(0);
            let delta = &value["delta"];
            match delta["type"].as_str() {
                Some("text_delta") => {
                    let text = delta["text"].as_str().unwrap_or_default();
                    if !text.is_empty() {
                        out.push_back(Ok(StreamEvent::ContentDelta(text.to_string())));
                    }
                }
                Some("input_json_delta") => {
                    let partial = delta["partial_json"].as_str().unwrap_or_default();
                    if let Some((id, _, buf)) = state.open_tool_blocks.get_mut(&index) {
                        buf.push_str(partial);
                        if !partial.is_empty() {
                            out.push_back(Ok(StreamEvent::ToolCallDelta {
                                id: id.clone(),
                                delta: partial.to_string(),
                            }));
                        }
                    }
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            let index = value["index"].as_u64().unwrap_or(0);
            if let Some((id, name, buf)) = state.open_tool_blocks.remove(&index) {
                let arguments = serde_json::from_str(&buf).unwrap_or(serde_json::Value::Null);
                state.finished_calls.push(ToolCall {
                    id,
                    name,
                    arguments,
                });
            }
        }
        "message_delta" => {
            let stop_reason = value["delta"]["stop_reason"].as_str();
            let output_tokens = value["usage"]["output_tokens"].as_u64().unwrap_or(0) as u32;
            out.push_back(Ok(StreamEvent::Usage(Usage {
                tokens_in: state.input_tokens,
                tokens_out: output_tokens,
                cost_cents: None,
            })));
            state.pending_finish = Some(map_stop_reason(stop_reason, state.finished_calls.clone()));
        }
        "message_stop" => {
            if let Some(reason) = state.pending_finish.take() {
                out.push_back(Ok(StreamEvent::Finish(reason)));
            }
        }
        // "ping" and any other event carry nothing this layer surfaces.
        _ => {}
    }
}

/// Map an `"exception"`-typed event-stream message onto a [`ProviderError`].
/// Bedrock names the exception on `:exception-type` and carries a
/// human-readable reason in the JSON payload's `message` field.
fn exception_to_error(msg: &Message) -> ProviderError {
    let kind = msg
        .headers
        .get(":exception-type")
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());
    let text = std::str::from_utf8(&msg.payload).unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| text.to_string());
    match kind.as_str() {
        "throttlingException" => ProviderError::RateLimited { retry_after_ms: 0 },
        "validationException" => ProviderError::InvalidRequest(message),
        // modelStreamErrorException, internalServerException, and any
        // exception type Bedrock adds later all fall through here.
        _ => ProviderError::Upstream(format!("{kind}: {message}")),
    }
}

/// Decode one event-stream [`Message`] into zero-or-more shared
/// [`StreamEvent`]s, or a terminal error for an `"exception"` message.
fn process_message(
    msg: Message,
    state: &mut AnthropicEventState,
    out: &mut VecDeque<Result<StreamEvent, ProviderError>>,
) -> bool {
    match msg.headers.get(":message-type").map(String::as_str) {
        Some("exception") => {
            out.push_back(Err(exception_to_error(&msg)));
            true // terminal
        }
        _ => {
            // A "chunk" event's payload is `{"bytes": "<base64>"}`; anything
            // else (an unrecognized event-type) is skipped rather than
            // treated as fatal, so a future event Bedrock adds doesn't break
            // an otherwise-healthy stream.
            if let Ok(wrapper) = serde_json::from_slice::<serde_json::Value>(&msg.payload) {
                if let Some(b64) = wrapper["bytes"].as_str() {
                    if let Ok(decoded) = BASE64.decode(b64) {
                        if let Ok(text) = String::from_utf8(decoded) {
                            process_anthropic_event(&text, state, out);
                        }
                    }
                }
            }
            false
        }
    }
}

struct StreamState {
    bytes: Pin<Box<dyn Stream<Item = Result<Vec<u8>, reqwest::Error>> + Send>>,
    buf: Vec<u8>,
    pending: VecDeque<Result<StreamEvent, ProviderError>>,
    anthropic_state: AnthropicEventState,
    finished: bool,
}

fn drain_frames(st: &mut StreamState) {
    loop {
        match eventstream::decode_frame(&st.buf) {
            Ok(Some((msg, consumed))) => {
                st.buf.drain(..consumed);
                let terminal = process_message(msg, &mut st.anthropic_state, &mut st.pending);
                if terminal {
                    st.finished = true;
                    return;
                }
            }
            Ok(None) => return, // wait for more bytes
            Err(e) => {
                st.pending
                    .push_back(Err(ProviderError::Upstream(e.to_string())));
                st.finished = true;
                return;
            }
        }
    }
}

/// Turn a live, 2xx [`reqwest::Response`] (an
/// `application/vnd.amazon.eventstream` body from
/// `InvokeModelWithResponseStream`) into the shared [`StreamEvent`] feed
/// [`Provider::stream`](ardur_provider_runtime::Provider::stream) yields.
pub(crate) fn into_provider_events(
    resp: reqwest::Response,
) -> impl Stream<Item = Result<StreamEvent, ProviderError>> + Send {
    let bytes = resp.bytes_stream().map(|r| r.map(|b| b.to_vec()));
    let state = StreamState {
        bytes: Box::pin(bytes),
        buf: Vec::new(),
        pending: VecDeque::new(),
        anthropic_state: AnthropicEventState::default(),
        finished: false,
    };

    futures::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(item) = st.pending.pop_front() {
                return Some((item, st));
            }
            if st.finished {
                return None;
            }
            match st.bytes.next().await {
                Some(Ok(chunk)) => {
                    st.buf.extend_from_slice(&chunk);
                    drain_frames(&mut st);
                }
                Some(Err(e)) => {
                    st.pending
                        .push_back(Err(ProviderError::NetworkFailure(e.to_string())));
                    st.finished = true;
                }
                None => {
                    st.finished = true;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events_of(json: &str) -> Vec<StreamEvent> {
        let mut state = AnthropicEventState::default();
        let mut out = VecDeque::new();
        process_anthropic_event(json, &mut state, &mut out);
        out.into_iter().map(|r| r.unwrap()).collect()
    }

    #[test]
    fn text_delta_becomes_content_event() {
        let events = events_of(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"pong"}}"#,
        );
        assert_eq!(events, vec![StreamEvent::ContentDelta("pong".to_string())]);
    }

    #[test]
    fn full_tool_use_block_assembles_across_events() {
        let mut state = AnthropicEventState::default();
        let mut out = VecDeque::new();
        process_anthropic_event(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":9}}}"#,
            &mut state,
            &mut out,
        );
        process_anthropic_event(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"echo"}}"#,
            &mut state,
            &mut out,
        );
        process_anthropic_event(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"msg\":"}}"#,
            &mut state,
            &mut out,
        );
        process_anthropic_event(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"hi\"}"}}"#,
            &mut state,
            &mut out,
        );
        process_anthropic_event(
            r#"{"type":"content_block_stop","index":0}"#,
            &mut state,
            &mut out,
        );
        process_anthropic_event(
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}"#,
            &mut state,
            &mut out,
        );
        process_anthropic_event(r#"{"type":"message_stop"}"#, &mut state, &mut out);

        let events: Vec<StreamEvent> = out.into_iter().map(|r| r.unwrap()).collect();
        assert!(
            matches!(&events[0], StreamEvent::ToolCallStart(c) if c.id == "toolu_1" && c.name == "echo")
        );
        assert!(matches!(&events[1], StreamEvent::ToolCallDelta { id, .. } if id == "toolu_1"));
        assert!(matches!(&events[2], StreamEvent::ToolCallDelta { id, .. } if id == "toolu_1"));
        assert!(
            matches!(&events[3], StreamEvent::Usage(u) if u.tokens_in == 9 && u.tokens_out == 4)
        );
        match &events[4] {
            StreamEvent::Finish(FinishReason::ToolUse(calls)) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "toolu_1");
                assert_eq!(calls[0].arguments, serde_json::json!({"msg": "hi"}));
            }
            other => panic!("expected Finish(ToolUse), got {other:?}"),
        }
    }

    #[test]
    fn end_turn_stop_reason_maps_to_finish_stop() {
        let mut state = AnthropicEventState::default();
        let mut out = VecDeque::new();
        process_anthropic_event(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
            &mut state,
            &mut out,
        );
        process_anthropic_event(r#"{"type":"message_stop"}"#, &mut state, &mut out);
        let events: Vec<StreamEvent> = out.into_iter().map(|r| r.unwrap()).collect();
        assert!(matches!(
            events.last(),
            Some(StreamEvent::Finish(FinishReason::Stop))
        ));
    }

    #[test]
    fn exception_message_becomes_terminal_error() {
        let frame = super::super::eventstream::tests::build_frame(
            &[
                (":message-type", "exception"),
                (":exception-type", "validationException"),
            ],
            br#"{"message":"bad request"}"#,
        );
        let (msg, _) = eventstream::decode_frame(&frame).unwrap().unwrap();
        let mut state = AnthropicEventState::default();
        let mut out = VecDeque::new();
        let terminal = process_message(msg, &mut state, &mut out);
        assert!(terminal);
        assert!(matches!(out[0], Err(ProviderError::InvalidRequest(_))));
    }

    #[test]
    fn chunk_frame_round_trips_through_process_message() {
        let inner =
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#;
        let b64 = BASE64.encode(inner);
        let payload = format!(r#"{{"bytes":"{b64}"}}"#);
        let frame = super::super::eventstream::tests::build_frame(
            &[(":message-type", "event"), (":event-type", "chunk")],
            payload.as_bytes(),
        );
        let (msg, _) = eventstream::decode_frame(&frame).unwrap().unwrap();
        let mut state = AnthropicEventState::default();
        let mut out = VecDeque::new();
        let terminal = process_message(msg, &mut state, &mut out);
        assert!(!terminal);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], Ok(StreamEvent::ContentDelta(t)) if t == "hi"));
    }
}
