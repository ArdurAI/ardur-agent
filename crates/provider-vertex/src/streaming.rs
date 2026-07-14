//! §3.4 Phase 2 — real incremental SSE streaming for the Vertex (Gemini)
//! backend.
//!
//! Vertex's `streamGenerateContent` endpoint, called with `?alt=sse`, answers
//! with a `text/event-stream` of `data: {json}\n\n` lines (no terminal
//! `[DONE]` marker — the stream just ends when the connection closes). Each
//! JSON payload is a `GenerateContentResponse`-shaped fragment: unlike
//! OpenAI/Azure's character-by-character tool-call streaming, Gemini emits a
//! `functionCall` part whole in one chunk (no incremental argument
//! fragments), and typically bundles `usageMetadata` together with the final
//! chunk's `finishReason` rather than as a separate trailing chunk.
//!
//! This module decodes that feed directly into the shared
//! [`StreamEvent`](ardur_provider_runtime::StreamEvent) protocol. A
//! `functionCall` part is surfaced as a
//! [`ToolCallStart`](StreamEvent::ToolCallStart) (empty arguments, per the
//! shared protocol's convention) immediately followed by one
//! [`ToolCallDelta`](StreamEvent::ToolCallDelta) carrying the whole
//! arguments JSON as a single fragment — Gemini gives the whole call in one
//! shot, so there is nothing to split further.
//!
//! Cancellation is by drop: dropping the returned stream drops the
//! underlying `reqwest` byte stream, closing the HTTP connection.

use std::collections::VecDeque;
use std::pin::Pin;

use ardur_provider_runtime::{FinishReason, ProviderError, StreamEvent, ToolCall, Usage};
use futures::{Stream, StreamExt};
use serde::Deserialize;

#[derive(Deserialize)]
struct GenerateContentResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
    #[serde(default)]
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<ApiUsage>,
}

#[derive(Deserialize)]
struct Candidate {
    #[serde(default)]
    content: Option<CandidateContent>,
    #[serde(default)]
    #[serde(rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct CandidateContent {
    #[serde(default)]
    parts: Vec<CandidatePart>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CandidatePart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    function_call: Option<FunctionCallPart>,
}

#[derive(Deserialize)]
struct FunctionCallPart {
    name: String,
    #[serde(default)]
    args: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiUsage {
    #[serde(default)]
    prompt_token_count: u32,
    #[serde(default)]
    candidates_token_count: u32,
}

/// Decode one streamed JSON chunk into zero-or-more shared [`StreamEvent`]s.
/// `next_call_index` numbers synthesized tool-call ids across the whole
/// stream (Gemini function calls carry no id of their own — see the
/// non-streaming path's identical convention).
fn process_chunk(
    json: &str,
    next_call_index: &mut usize,
    pending_finish: &mut Option<FinishReason>,
    out: &mut VecDeque<Result<StreamEvent, ProviderError>>,
) {
    let parsed: GenerateContentResponse = match serde_json::from_str(json) {
        Ok(p) => p,
        Err(e) => {
            out.push_back(Err(ProviderError::Upstream(format!(
                "unexpected stream chunk shape: {e}"
            ))));
            return;
        }
    };

    if let Some(candidate) = parsed.candidates.into_iter().next() {
        let parts = candidate.content.unwrap_or_default().parts;
        let mut tool_calls = Vec::new();
        for part in parts {
            if let Some(text) = part.text {
                if !text.is_empty() {
                    out.push_back(Ok(StreamEvent::ContentDelta(text)));
                }
            }
            if let Some(fc) = part.function_call {
                let id = format!("call_{next_call_index}");
                *next_call_index += 1;
                out.push_back(Ok(StreamEvent::ToolCallStart(ToolCall {
                    id: id.clone(),
                    name: fc.name.clone(),
                    arguments: serde_json::Value::Null,
                })));
                out.push_back(Ok(StreamEvent::ToolCallDelta {
                    id: id.clone(),
                    delta: fc.args.to_string(),
                }));
                tool_calls.push(ToolCall {
                    id,
                    name: fc.name,
                    arguments: fc.args,
                });
            }
        }

        if let Some(reason) = candidate.finish_reason {
            *pending_finish = Some(map_finish_reason(&reason, tool_calls));
        }
    }

    if let Some(u) = parsed.usage_metadata {
        out.push_back(Ok(StreamEvent::Usage(Usage {
            tokens_in: u.prompt_token_count,
            tokens_out: u.candidates_token_count,
            cost_cents: None,
        })));
        flush_pending_finish(pending_finish, out);
    }
}

fn map_finish_reason(reason: &str, tool_calls: Vec<ToolCall>) -> FinishReason {
    if !tool_calls.is_empty() {
        return FinishReason::ToolUse(tool_calls);
    }
    match reason {
        "STOP" => FinishReason::Stop,
        "MAX_TOKENS" => FinishReason::MaxTokens,
        "SAFETY" | "RECITATION" => FinishReason::Error(format!("generation halted: {reason}")),
        other => FinishReason::Error(format!("unknown finishReason: {other}")),
    }
}

fn flush_pending_finish(
    pending_finish: &mut Option<FinishReason>,
    out: &mut VecDeque<Result<StreamEvent, ProviderError>>,
) {
    if let Some(reason) = pending_finish.take() {
        out.push_back(Ok(StreamEvent::Finish(reason)));
    }
}

struct StreamState {
    bytes: Pin<Box<dyn Stream<Item = Result<Vec<u8>, reqwest::Error>> + Send>>,
    buf: Vec<u8>,
    pending: VecDeque<Result<StreamEvent, ProviderError>>,
    next_call_index: usize,
    pending_finish: Option<FinishReason>,
    finished: bool,
}

fn handle_sse_line(line: &str, st: &mut StreamState) {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() || line.starts_with(':') {
        return;
    }
    let Some(rest) = line.strip_prefix("data:") else {
        return;
    };
    let payload = rest.trim_start();
    if payload.is_empty() {
        return;
    }
    process_chunk(
        payload,
        &mut st.next_call_index,
        &mut st.pending_finish,
        &mut st.pending,
    );
}

fn drain_lines(st: &mut StreamState) {
    while let Some(pos) = st.buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = st.buf.drain(..=pos).collect();
        if let Ok(text) = std::str::from_utf8(&line) {
            handle_sse_line(text, st);
        }
    }
}

/// Turn a live, 2xx [`reqwest::Response`] (a `text/event-stream` from
/// `streamGenerateContent?alt=sse`) into the shared [`StreamEvent`] feed
/// [`Provider::stream`](ardur_provider_runtime::Provider::stream) yields.
pub(crate) fn into_provider_events(
    resp: reqwest::Response,
) -> impl Stream<Item = Result<StreamEvent, ProviderError>> + Send {
    let bytes = resp.bytes_stream().map(|r| r.map(|b| b.to_vec()));
    let state = StreamState {
        bytes: Box::pin(bytes),
        buf: Vec::new(),
        pending: VecDeque::new(),
        next_call_index: 0,
        pending_finish: None,
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
                    drain_lines(&mut st);
                }
                Some(Err(e)) => {
                    st.pending
                        .push_back(Err(ProviderError::NetworkFailure(e.to_string())));
                    st.finished = true;
                }
                None => {
                    if !st.buf.is_empty() {
                        let trailing = std::mem::take(&mut st.buf);
                        if let Ok(text) = std::str::from_utf8(&trailing) {
                            handle_sse_line(text, &mut st);
                        }
                    }
                    flush_pending_finish(&mut st.pending_finish, &mut st.pending);
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
        let mut next_call_index = 0;
        let mut pending_finish = None;
        let mut out = VecDeque::new();
        process_chunk(json, &mut next_call_index, &mut pending_finish, &mut out);
        flush_pending_finish(&mut pending_finish, &mut out);
        out.into_iter().map(|r| r.unwrap()).collect()
    }

    #[test]
    fn content_delta_becomes_content_event() {
        let events =
            events_of(r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"pong"}]}}]}"#);
        assert_eq!(events, vec![StreamEvent::ContentDelta("pong".to_string())]);
    }

    #[test]
    fn finish_reason_and_usage_in_same_chunk_flushes_together() {
        let events = events_of(
            r#"{"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1}}"#,
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::Usage(Usage {
                    tokens_in: 3,
                    tokens_out: 1,
                    cost_cents: None
                }),
                StreamEvent::Finish(FinishReason::Stop),
            ]
        );
    }

    #[test]
    fn function_call_part_becomes_start_then_delta() {
        let events = events_of(
            r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"echo","args":{"msg":"hi"}}}]},"finishReason":"STOP"}]}"#,
        );
        assert!(
            matches!(&events[0], StreamEvent::ToolCallStart(c) if c.id == "call_0" && c.name == "echo")
        );
        assert!(matches!(&events[1], StreamEvent::ToolCallDelta { id, .. } if id == "call_0"));
        assert!(matches!(
            &events[2],
            StreamEvent::Finish(FinishReason::ToolUse(calls)) if calls.len() == 1 && calls[0].name == "echo"
        ));
    }

    #[test]
    fn malformed_chunk_yields_upstream_error() {
        let mut next_call_index = 0;
        let mut pending_finish = None;
        let mut out = VecDeque::new();
        process_chunk(
            "not json",
            &mut next_call_index,
            &mut pending_finish,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Err(ProviderError::Upstream(_))));
    }
}
