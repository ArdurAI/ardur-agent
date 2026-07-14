//! §3.4 Phase 2 — real incremental SSE streaming for the Azure OpenAI backend.
//!
//! Azure's Chat Completions streaming wire format is byte-for-byte identical to
//! OpenAI's (the transport differences — resource/deployment URL, `api-key`
//! header — only affect the connect, not the SSE framing): `POST` with
//! `stream: true` answers with a `text/event-stream` of `data: {json}\n\n`
//! lines, terminated by a literal `data: [DONE]\n\n`. Each JSON payload's
//! `choices[0].delta` carries incremental text and tool-call fragments keyed by
//! integer `index`.
//!
//! This module decodes that feed directly into the shared
//! [`StreamEvent`](ardur_provider_runtime::StreamEvent) protocol in one pass
//! (no separate public chunk type — nothing outside [`crate::Provider::stream`]
//! needs the raw wire shape). Tool-call fragments are remapped from
//! OpenAI/Azure's index-keyed scheme to the shared protocol's id-keyed
//! `ToolCallStart`/`ToolCallDelta` events, buffering any fragment that arrives
//! before its call's `id` is known (in practice the id always arrives first).
//!
//! Cancellation is by drop: dropping the returned stream drops the underlying
//! `reqwest` byte stream, closing the HTTP connection.

use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;

use ardur_provider_runtime::{FinishReason, ProviderError, StreamEvent, ToolCall, Usage};
use futures::{Stream, StreamExt};
use serde::Deserialize;

/// The streamed-chunk shape: a partial of the chat-completions response whose
/// `choices[0]` carries a `delta` rather than a finished `message`.
#[derive(Debug, Deserialize)]
struct ChatCompletionStreamResponse {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<StreamUsage>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<DeltaToolCall>,
}

#[derive(Debug, Deserialize)]
struct DeltaToolCall {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<DeltaFunction>,
}

#[derive(Debug, Deserialize)]
struct DeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

/// Tracks, per streamed tool-call `index`, the `id` once known (so an
/// `arguments` fragment that races ahead of its id can be buffered rather than
/// emitted with a placeholder), and the accumulated arguments string per index
/// for the final [`FinishReason::ToolUse`] assembly.
#[derive(Debug, Default)]
struct ToolCallState {
    ids: BTreeMap<u32, String>,
    names: BTreeMap<u32, String>,
    arguments: BTreeMap<u32, String>,
    pending_fragments: BTreeMap<u32, Vec<String>>,
}

impl ToolCallState {
    fn finalize(&self) -> Vec<ToolCall> {
        self.ids
            .iter()
            .map(|(index, id)| ToolCall {
                id: id.clone(),
                name: self.names.get(index).cloned().unwrap_or_default(),
                arguments: self
                    .arguments
                    .get(index)
                    .map(|s| serde_json::from_str(s).unwrap_or(serde_json::Value::Null))
                    .unwrap_or(serde_json::Value::Null),
            })
            .collect()
    }

    /// Fold one wire tool-call delta in, pushing any shared [`StreamEvent`]s it
    /// produces onto `out`.
    fn ingest(
        &mut self,
        tc: &DeltaToolCall,
        out: &mut VecDeque<Result<StreamEvent, ProviderError>>,
    ) {
        let index = tc.index;
        let name = tc
            .function
            .as_ref()
            .and_then(|f| f.name.clone())
            .filter(|s| !s.is_empty());
        let arguments = tc
            .function
            .as_ref()
            .and_then(|f| f.arguments.clone())
            .unwrap_or_default();

        if let Some(id) = tc.id.as_deref().filter(|s| !s.is_empty()) {
            if !self.ids.contains_key(&index) {
                if let Some(n) = &name {
                    self.names.insert(index, n.clone());
                }
                out.push_back(Ok(StreamEvent::ToolCallStart(ToolCall {
                    id: id.to_string(),
                    name: name.clone().unwrap_or_default(),
                    arguments: serde_json::Value::Null,
                })));
                self.ids.insert(index, id.to_string());
                if let Some(buffered) = self.pending_fragments.remove(&index) {
                    for frag in buffered {
                        self.arguments.entry(index).or_default().push_str(&frag);
                        out.push_back(Ok(StreamEvent::ToolCallDelta {
                            id: id.to_string(),
                            delta: frag,
                        }));
                    }
                }
            }
        }

        if !arguments.is_empty() {
            match self.ids.get(&index) {
                Some(id) => {
                    self.arguments
                        .entry(index)
                        .or_default()
                        .push_str(&arguments);
                    out.push_back(Ok(StreamEvent::ToolCallDelta {
                        id: id.clone(),
                        delta: arguments,
                    }));
                }
                None => self
                    .pending_fragments
                    .entry(index)
                    .or_default()
                    .push(arguments),
            }
        }
    }
}

/// Decode one streamed JSON chunk into zero-or-more shared [`StreamEvent`]s.
fn process_chunk(
    json: &str,
    tool_state: &mut ToolCallState,
    pending_finish: &mut Option<FinishReason>,
    out: &mut VecDeque<Result<StreamEvent, ProviderError>>,
) {
    let parsed: ChatCompletionStreamResponse = match serde_json::from_str(json) {
        Ok(p) => p,
        Err(e) => {
            out.push_back(Err(ProviderError::Upstream(format!(
                "unexpected stream chunk shape: {e}"
            ))));
            return;
        }
    };

    if let Some(choice) = parsed.choices.into_iter().next() {
        if let Some(content) = choice.delta.content {
            if !content.is_empty() {
                out.push_back(Ok(StreamEvent::ContentDelta(content)));
            }
        }
        for tc in &choice.delta.tool_calls {
            tool_state.ingest(tc, out);
        }
        if let Some(reason) = choice.finish_reason {
            *pending_finish = Some(match reason.as_str() {
                "stop" => FinishReason::Stop,
                "length" => FinishReason::MaxTokens,
                "tool_calls" => FinishReason::ToolUse(tool_state.finalize()),
                "content_filter" => {
                    FinishReason::Error("generation halted by content filter".to_string())
                }
                other => FinishReason::Error(format!("unknown finish_reason: {other}")),
            });
        }
    }

    if let Some(u) = parsed.usage {
        out.push_back(Ok(StreamEvent::Usage(Usage {
            tokens_in: u.prompt_tokens,
            tokens_out: u.completion_tokens,
            cost_cents: None,
        })));
        flush_pending_finish(pending_finish, out);
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
    tool_state: ToolCallState,
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
    if payload == "[DONE]" {
        flush_pending_finish(&mut st.pending_finish, &mut st.pending);
        st.finished = true;
        return;
    }
    process_chunk(
        payload,
        &mut st.tool_state,
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
        if st.finished {
            break;
        }
    }
}

/// Turn a live, 2xx [`reqwest::Response`] (a `text/event-stream`) into the
/// shared [`StreamEvent`] feed [`Provider::stream`](ardur_provider_runtime::Provider::stream)
/// yields.
pub(crate) fn into_provider_events(
    resp: reqwest::Response,
) -> impl Stream<Item = Result<StreamEvent, ProviderError>> + Send {
    let bytes = resp.bytes_stream().map(|r| r.map(|b| b.to_vec()));
    let state = StreamState {
        bytes: Box::pin(bytes),
        buf: Vec::new(),
        pending: VecDeque::new(),
        tool_state: ToolCallState::default(),
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
        let mut tool_state = ToolCallState::default();
        let mut pending_finish = None;
        let mut out = VecDeque::new();
        process_chunk(json, &mut tool_state, &mut pending_finish, &mut out);
        flush_pending_finish(&mut pending_finish, &mut out);
        out.into_iter().map(|r| r.unwrap()).collect()
    }

    #[test]
    fn content_delta_becomes_content_event() {
        let events = events_of(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#);
        assert_eq!(events, vec![StreamEvent::ContentDelta("Hel".to_string())]);
    }

    #[test]
    fn finish_reason_stop_becomes_finish_event() {
        let events = events_of(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#);
        assert_eq!(events, vec![StreamEvent::Finish(FinishReason::Stop)]);
    }

    #[test]
    fn usage_flushes_pending_finish_after_it() {
        let events = events_of(
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#,
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
    fn tool_call_fragments_assemble_into_finish_tool_use() {
        let mut tool_state = ToolCallState::default();
        let mut pending_finish = None;
        let mut out = VecDeque::new();
        process_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"echo","arguments":""}}]}}]}"#,
            &mut tool_state,
            &mut pending_finish,
            &mut out,
        );
        process_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"msg\""}}]}}]}"#,
            &mut tool_state,
            &mut pending_finish,
            &mut out,
        );
        process_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"hi\"}"}}]}}]}"#,
            &mut tool_state,
            &mut pending_finish,
            &mut out,
        );
        process_chunk(
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            &mut tool_state,
            &mut pending_finish,
            &mut out,
        );
        flush_pending_finish(&mut pending_finish, &mut out);

        let events: Vec<StreamEvent> = out.into_iter().map(|r| r.unwrap()).collect();
        assert!(
            matches!(&events[0], StreamEvent::ToolCallStart(c) if c.id == "call_1" && c.name == "echo")
        );
        assert!(
            matches!(&events[1], StreamEvent::ToolCallDelta { id, delta } if id == "call_1" && delta == "{\"msg\"")
        );
        assert!(
            matches!(&events[2], StreamEvent::ToolCallDelta { id, delta } if id == "call_1" && delta == ":\"hi\"}")
        );
        match &events[3] {
            StreamEvent::Finish(FinishReason::ToolUse(calls)) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "call_1");
                assert_eq!(calls[0].name, "echo");
                assert_eq!(calls[0].arguments, serde_json::json!({"msg": "hi"}));
            }
            other => panic!("expected Finish(ToolUse), got {other:?}"),
        }
    }

    #[test]
    fn malformed_chunk_yields_upstream_error() {
        let mut tool_state = ToolCallState::default();
        let mut pending_finish = None;
        let mut out = VecDeque::new();
        process_chunk("not json", &mut tool_state, &mut pending_finish, &mut out);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Err(ProviderError::Upstream(_))));
    }
}
