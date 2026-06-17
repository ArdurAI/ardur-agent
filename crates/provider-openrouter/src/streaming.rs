//! §3.2b — SSE streaming for the OpenRouter (OpenAI-compatible) backend.
//!
//! OpenRouter follows OpenAI's Chat Completions *streaming* wire format
//! byte-for-byte: a `POST /chat/completions` with `stream: true` answers with a
//! `text/event-stream` of server-sent events, each a line
//! `data: {json}\n\n`, the feed terminated by a literal `data: [DONE]\n\n`.
//! Every JSON payload is a [`ChatCompletionStreamResponse`] — a partial of the
//! non-streaming response whose `choices[0].delta` carries the *incremental*
//! text and tool-call fragments rather than a finished `message`.
//!
//! This module owns three things:
//!
//! - the wire structs that deserialize one streamed chunk;
//! - [`ToolCallAccumulator`], which stitches the per-`index` `function.arguments`
//!   fragments OpenAI streams across many chunks back into whole [`ToolCall`]s,
//!   finalized when a `finish_reason: "tool_calls"` chunk arrives;
//! - the [`OpenRouterChunk`] event the public stream yields and the
//!   [`into_chunk_stream`] adapter that turns a live [`reqwest::Response`]'s byte
//!   feed into that event stream (a [`futures::stream::unfold`] loop over a small
//!   line buffer — no `eventsource` crate, the framing is line-trivial).
//!
//! Cancellation is by drop: dropping the returned stream drops the underlying
//! [`reqwest`] byte stream, which closes the HTTP connection.

use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;

use ardur_provider_runtime::{FinishReason, ProviderError, StreamEvent, ToolCall, Usage};
use futures::{Stream, StreamExt};
use serde::Deserialize;

/// One event yielded by the [`stream_chat`](crate::OpenRouterProvider::stream_chat)
/// stream.
///
/// The stream interleaves these as the upstream produces them: any number of
/// [`Content`](Self::Content) / [`ToolCall`](Self::ToolCall) deltas, then a
/// single [`Done`](Self::Done) when the choice's `finish_reason` arrives, then —
/// when `stream_options.include_usage` was requested — a final
/// [`Usage`](Self::Usage). The stream ends (yields `None`) at the `[DONE]`
/// marker.
#[derive(Clone, Debug, PartialEq)]
pub enum OpenRouterChunk {
    /// An incremental slice of assistant text (`choices[0].delta.content`).
    Content(String),
    /// An incremental tool-call fragment (`choices[0].delta.tool_calls[i]`).
    ///
    /// OpenAI streams a tool call across several chunks: the first carries the
    /// `id` and `function.name`, later ones append `function.arguments`
    /// fragments. These raw deltas are surfaced for live UIs; the provider also
    /// feeds them to an internal [`ToolCallAccumulator`], so the
    /// fully-reconstructed calls are delivered in the
    /// [`Done`](Self::Done)`(FinishReason::ToolUse(..))` event — most callers can
    /// ignore the raw deltas and read the assembled calls there.
    ToolCall(ToolCallDelta),
    /// Final token usage, present on the last data chunk before `[DONE]` when the
    /// request set `stream_options.include_usage` (which `stream_chat` does).
    Usage(Usage),
    /// The choice finished. The [`FinishReason`] is mapped from the upstream
    /// `finish_reason`; for `"tool_calls"` it carries the assembled
    /// [`ToolCall`]s ([`FinishReason::ToolUse`]).
    Done(FinishReason),
}

/// An incremental tool-call fragment from a streamed `delta.tool_calls[i]`.
///
/// `id` and `name` appear (non-empty) on the first fragment for an `index`;
/// `arguments` is the partial JSON slice carried by *this* chunk (often empty on
/// the opening fragment, a few characters on each following one). Feed a sequence
/// of these for the same `index` into a [`ToolCallAccumulator`] to rebuild the
/// whole call.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolCallDelta {
    /// Which tool call (by position) this fragment belongs to. OpenAI keys
    /// streamed tool calls by this index, not by `id`.
    pub index: u32,
    /// The call id, present (non-empty) only on the opening fragment.
    pub id: Option<String>,
    /// The tool name, present (non-empty) only on the opening fragment.
    pub name: Option<String>,
    /// The partial `function.arguments` JSON carried by this chunk (possibly
    /// empty); concatenating these across fragments yields the whole argument
    /// JSON string.
    pub arguments: String,
}

/// Stitches the per-`index` tool-call fragments OpenAI streams back into whole
/// [`ToolCall`]s.
///
/// Feed every [`ToolCallDelta`] for a turn into [`ingest`](Self::ingest) in
/// arrival order, then call [`finalize`](Self::finalize) once the
/// `finish_reason: "tool_calls"` chunk lands. `ingest` keeps the last non-empty
/// `id`/`name` seen per index and appends every `arguments` fragment; `finalize`
/// parses each accumulated argument string into a JSON value (falling back to
/// [`Value::Null`](serde_json::Value::Null) if a call streamed no/!valid JSON).
/// Calls come out ordered by `index`.
#[derive(Clone, Debug, Default)]
pub struct ToolCallAccumulator {
    calls: BTreeMap<u32, PartialToolCall>,
}

/// The mutable per-index state a [`ToolCallAccumulator`] builds up.
#[derive(Clone, Debug, Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCallAccumulator {
    /// A fresh, empty accumulator.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one streamed fragment in: remember a non-empty `id`/`name` and append
    /// the `arguments` slice to the call at `delta.index`.
    pub fn ingest(&mut self, delta: &ToolCallDelta) {
        let entry = self.calls.entry(delta.index).or_default();
        if let Some(id) = delta.id.as_deref() {
            if !id.is_empty() {
                entry.id = id.to_string();
            }
        }
        if let Some(name) = delta.name.as_deref() {
            if !name.is_empty() {
                entry.name = name.to_string();
            }
        }
        entry.arguments.push_str(&delta.arguments);
    }

    /// Reconstruct the whole tool calls seen so far, ordered by index. Each
    /// call's accumulated `arguments` string is parsed as JSON; an empty or
    /// malformed buffer decodes to [`Value::Null`](serde_json::Value::Null) (the
    /// same lenient decode the non-streaming path uses).
    #[must_use]
    pub fn finalize(&self) -> Vec<ToolCall> {
        self.calls
            .values()
            .map(|p| ToolCall {
                id: p.id.clone(),
                name: p.name.clone(),
                arguments: serde_json::from_str(&p.arguments).unwrap_or(serde_json::Value::Null),
            })
            .collect()
    }

    /// Whether no tool-call fragment has been ingested yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
}

/// The streamed-chunk shape: a partial of the chat-completions response whose
/// `choices[0]` carries a `delta` rather than a finished `message`, plus the
/// optional final `usage`.
#[derive(Debug, Deserialize)]
struct ChatCompletionStreamResponse {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<StreamUsage>,
}

/// One entry of a streamed `choices` array.
#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

/// The incremental `delta` inside a streamed [`StreamChoice`].
#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<DeltaToolCall>,
}

/// One streamed `delta.tool_calls[i]` entry (OpenAI shape).
#[derive(Debug, Deserialize)]
struct DeltaToolCall {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<DeltaFunction>,
}

/// The streamed `function` object: name on the opening fragment, an arguments
/// slice on each.
#[derive(Debug, Deserialize)]
struct DeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// The streamed `usage` object (final chunk). Only token counts are surfaced;
/// streamed chunks do not carry OpenRouter's per-call `cost`.
#[derive(Debug, Deserialize)]
struct StreamUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

/// Lift one streamed wire tool-call into the public [`ToolCallDelta`], dropping
/// empty `id`/`name` (which mark "no value on this fragment").
fn delta_from_wire(tc: &DeltaToolCall) -> ToolCallDelta {
    let (name, arguments) = match &tc.function {
        Some(f) => (
            f.name.clone().filter(|s| !s.is_empty()),
            f.arguments.clone().unwrap_or_default(),
        ),
        None => (None, String::new()),
    };
    ToolCallDelta {
        index: tc.index,
        id: tc.id.clone().filter(|s| !s.is_empty()),
        name,
        arguments,
    }
}

/// Map a streamed `finish_reason` onto the crate's [`FinishReason`], finalizing
/// the accumulator into [`FinishReason::ToolUse`] for the `"tool_calls"` stop.
///
/// Mirrors the non-streaming `map_finish_reason`, but streaming always carries an
/// explicit reason on the terminal chunk (interim chunks send `null`, which the
/// caller never routes here).
fn map_stream_finish_reason(reason: &str, acc: &ToolCallAccumulator) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::MaxTokens,
        "tool_calls" => FinishReason::ToolUse(acc.finalize()),
        "content_filter" => FinishReason::Error("generation halted by content filter".to_string()),
        other => FinishReason::Error(format!("unknown finish_reason: {other}")),
    }
}

/// Decode one streamed JSON chunk into zero or more [`OpenRouterChunk`]s,
/// pushing them onto `out` and folding any tool-call fragments into `acc`.
///
/// A chunk may carry a content delta, tool-call deltas, a finish reason, and/or
/// usage; each present part becomes its own event, emitted content → tool-call →
/// done → usage. A chunk that fails to parse becomes a single
/// [`ProviderError::Upstream`].
fn process_chunk(
    json: &str,
    acc: &mut ToolCallAccumulator,
    out: &mut VecDeque<Result<OpenRouterChunk, ProviderError>>,
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
                out.push_back(Ok(OpenRouterChunk::Content(content)));
            }
        }
        for tc in &choice.delta.tool_calls {
            let delta = delta_from_wire(tc);
            acc.ingest(&delta);
            out.push_back(Ok(OpenRouterChunk::ToolCall(delta)));
        }
        if let Some(reason) = choice.finish_reason {
            out.push_back(Ok(OpenRouterChunk::Done(map_stream_finish_reason(
                &reason, acc,
            ))));
        }
    }

    if let Some(u) = parsed.usage {
        out.push_back(Ok(OpenRouterChunk::Usage(Usage {
            tokens_in: u.prompt_tokens,
            tokens_out: u.completion_tokens,
            cost_cents: None,
        })));
    }
}

/// The state the [`into_chunk_stream`] unfold threads across polls: the boxed
/// byte feed, a line buffer for bytes that don't yet form a whole SSE line, the
/// queue of decoded-but-unyielded events, the tool-call accumulator, and the
/// "saw `[DONE]` / upstream closed" flag.
struct StreamState {
    bytes: Pin<Box<dyn Stream<Item = Result<Vec<u8>, reqwest::Error>> + Send>>,
    buf: Vec<u8>,
    pending: VecDeque<Result<OpenRouterChunk, ProviderError>>,
    acc: ToolCallAccumulator,
    finished: bool,
}

/// Handle one complete SSE line (no trailing newline). Blank lines (event
/// boundaries), `:` comments, and non-`data:` fields (`event:`, `id:`) are
/// ignored; a `data: [DONE]` payload sets `finished`; any other `data:` payload
/// is a JSON chunk handed to [`process_chunk`].
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
        st.finished = true;
        return;
    }
    process_chunk(payload, &mut st.acc, &mut st.pending);
}

/// Pull every complete line (terminated by `\n`) out of the byte buffer and
/// dispatch it through [`handle_sse_line`], stopping early once `[DONE]` is seen.
fn drain_lines(st: &mut StreamState) {
    while let Some(pos) = st.buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = st.buf.drain(..=pos).collect();
        // Splitting on the `\n` byte is UTF-8-safe: 0x0A never appears inside a
        // multi-byte sequence, so a complete line is always valid UTF-8.
        if let Ok(text) = std::str::from_utf8(&line) {
            handle_sse_line(text, st);
        }
        if st.finished {
            break;
        }
    }
}

/// Turn a live, 2xx [`reqwest::Response`] (a `text/event-stream`) into the public
/// [`OpenRouterChunk`] event stream.
///
/// A [`futures::stream::unfold`] loop drives a [`StreamState`]: it drains queued
/// events first, then pulls the next byte chunk, buffers it, and decodes any
/// whole SSE lines it completes. A transport error becomes a terminal
/// [`ProviderError::NetworkFailure`]; `[DONE]` (or upstream EOF) ends the stream.
pub(crate) fn into_chunk_stream(
    resp: reqwest::Response,
) -> impl Stream<Item = Result<OpenRouterChunk, ProviderError>> + Send {
    // Convert each `Bytes` chunk to an owned `Vec<u8>` up front so the boxed
    // stream type names no `bytes::Bytes` (keeps `bytes` out of the dep set).
    let bytes = resp.bytes_stream().map(|r| r.map(|b| b.to_vec()));
    let state = StreamState {
        bytes: Box::pin(bytes),
        buf: Vec::new(),
        pending: VecDeque::new(),
        acc: ToolCallAccumulator::new(),
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
                    // Upstream closed. Flush any trailing line that arrived
                    // without a final newline, then end.
                    if !st.buf.is_empty() {
                        let trailing = std::mem::take(&mut st.buf);
                        if let Ok(text) = std::str::from_utf8(&trailing) {
                            handle_sse_line(text, &mut st);
                        }
                    }
                    st.finished = true;
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// §3.X — adapt the OpenRouter-native chunk feed onto the shared `StreamEvent`
// protocol.
// ---------------------------------------------------------------------------

/// Maps the index-keyed [`OpenRouterChunk`] feed onto the id-keyed shared
/// [`StreamEvent`] protocol.
///
/// OpenAI/OpenRouter streams tool calls keyed by an integer `index` and carries
/// each call's `id` only on its opening fragment, whereas the shared protocol
/// keys [`StreamEvent::ToolCallDelta`] by `id`. This adapter bridges the two: it
/// remembers the `id` learned for each `index` and, on the first fragment that
/// reveals it, emits a [`StreamEvent::ToolCallStart`].
///
/// **Out-of-order ids (the buffering choice).** If an `arguments` fragment
/// arrives *before* the fragment carrying its `id` (so the id is not yet known),
/// the fragment is **buffered** per index rather than emitted with a placeholder
/// id. Once the id lands, the [`StreamEvent::ToolCallStart`] is emitted first,
/// then the buffered fragments are flushed in arrival order as
/// [`StreamEvent::ToolCallDelta`]s. This keeps every delta correctly attributed
/// and the `start`-before-`delta` ordering the shared protocol promises — no
/// consumer ever sees an `id: ""` delta. (In practice OpenAI always sends the id
/// on the opening fragment, so the buffer stays empty; the path exists for
/// resilience to reordering.)
#[derive(Debug, Default)]
struct EventAdapter {
    /// `index` → the `id` once known. Presence also marks "start already emitted".
    ids: BTreeMap<u32, String>,
    /// `index` → argument fragments that arrived before the id was known.
    pending: BTreeMap<u32, Vec<String>>,
}

impl EventAdapter {
    /// Fold one [`OpenRouterChunk`] into zero-or-more shared [`StreamEvent`]s.
    fn adapt(
        &mut self,
        chunk: OpenRouterChunk,
        out: &mut VecDeque<Result<StreamEvent, ProviderError>>,
    ) {
        match chunk {
            OpenRouterChunk::Content(text) => {
                out.push_back(Ok(StreamEvent::ContentDelta(text)));
            }
            OpenRouterChunk::ToolCall(delta) => self.adapt_tool_call(delta, out),
            OpenRouterChunk::Usage(usage) => out.push_back(Ok(StreamEvent::Usage(usage))),
            OpenRouterChunk::Done(reason) => out.push_back(Ok(StreamEvent::Finish(reason))),
        }
    }

    /// Map one streamed tool-call fragment, emitting a [`StreamEvent::ToolCallStart`]
    /// the first time an `index`'s `id` is seen and routing the `arguments` slice
    /// to a [`StreamEvent::ToolCallDelta`] (buffering it if the id is not yet known).
    fn adapt_tool_call(
        &mut self,
        delta: ToolCallDelta,
        out: &mut VecDeque<Result<StreamEvent, ProviderError>>,
    ) {
        let ToolCallDelta {
            index,
            id,
            name,
            arguments,
        } = delta;

        // The opening fragment reveals the id (and name): emit the start once,
        // then flush any fragments that raced ahead of it.
        if let Some(id) = id.filter(|s| !s.is_empty()) {
            if !self.ids.contains_key(&index) {
                out.push_back(Ok(StreamEvent::ToolCallStart(ToolCall {
                    id: id.clone(),
                    name: name.unwrap_or_default(),
                    arguments: serde_json::Value::Null,
                })));
                if let Some(buffered) = self.pending.remove(&index) {
                    for frag in buffered {
                        out.push_back(Ok(StreamEvent::ToolCallDelta {
                            id: id.clone(),
                            delta: frag,
                        }));
                    }
                }
                self.ids.insert(index, id);
            }
        }

        // Route this fragment's argument slice — by id if known, else buffer it.
        if !arguments.is_empty() {
            match self.ids.get(&index) {
                Some(id) => out.push_back(Ok(StreamEvent::ToolCallDelta {
                    id: id.clone(),
                    delta: arguments,
                })),
                None => self.pending.entry(index).or_default().push(arguments),
            }
        }
    }
}

/// The state the [`into_provider_events`] unfold threads across polls: the source
/// chunk feed, the [`EventAdapter`] map, a queue of decoded-but-unyielded shared
/// events, and the "source drained" flag.
struct AdaptState<S> {
    chunks: S,
    adapter: EventAdapter,
    pending: VecDeque<Result<StreamEvent, ProviderError>>,
    finished: bool,
}

/// Adapt an [`OpenRouterChunk`] stream into the shared [`StreamEvent`] feed
/// [`Provider::stream`](ardur_provider_runtime::Provider::stream) yields (§3.X).
///
/// `Content` → [`ContentDelta`](StreamEvent::ContentDelta); `Usage` →
/// [`Usage`](StreamEvent::Usage); `Done` → [`Finish`](StreamEvent::Finish); and
/// `ToolCall` deltas are remapped from index-keyed to id-keyed by the
/// [`EventAdapter`] (with [`ToolCallStart`](StreamEvent::ToolCallStart) on the
/// first fragment per call). A mid-stream `Err` is forwarded unchanged.
pub(crate) fn into_provider_events<S>(
    chunks: S,
) -> impl Stream<Item = Result<StreamEvent, ProviderError>> + Send
where
    S: Stream<Item = Result<OpenRouterChunk, ProviderError>> + Send + 'static,
{
    let state = AdaptState {
        chunks: Box::pin(chunks),
        adapter: EventAdapter::default(),
        pending: VecDeque::new(),
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
            match st.chunks.next().await {
                Some(Ok(chunk)) => st.adapter.adapt(chunk, &mut st.pending),
                Some(Err(e)) => {
                    st.finished = true;
                    return Some((Err(e), st));
                }
                None => st.finished = true,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drain a single JSON chunk into the events it produces.
    fn events_of(json: &str) -> Vec<OpenRouterChunk> {
        let mut acc = ToolCallAccumulator::new();
        let mut out = VecDeque::new();
        process_chunk(json, &mut acc, &mut out);
        out.into_iter().map(|r| r.unwrap()).collect()
    }

    #[test]
    fn content_delta_becomes_content_event() {
        let events = events_of(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#);
        assert_eq!(events, vec![OpenRouterChunk::Content("Hel".to_string())]);
    }

    #[test]
    fn empty_content_delta_emits_nothing() {
        // The OpenAI opener carries `{"role":"assistant","content":""}` — no text.
        let events = events_of(r#"{"choices":[{"delta":{"role":"assistant","content":""}}]}"#);
        assert!(events.is_empty());
    }

    #[test]
    fn accumulator_stitches_arguments_across_fragments() {
        let mut acc = ToolCallAccumulator::new();
        acc.ingest(&ToolCallDelta {
            index: 0,
            id: Some("call_1".to_string()),
            name: Some("get_weather".to_string()),
            arguments: String::new(),
        });
        acc.ingest(&ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments: r#"{"loc"#.to_string(),
        });
        acc.ingest(&ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments: r#"ation":"SF"}"#.to_string(),
        });
        let calls = acc.finalize();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(calls[0].arguments, serde_json::json!({"location": "SF"}));
    }

    #[test]
    fn finish_reason_tool_calls_carries_assembled_calls() {
        let mut acc = ToolCallAccumulator::new();
        let mut out = VecDeque::new();
        // Opening fragment: id + name, empty args.
        process_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_9","function":{"name":"echo","arguments":""}}]}}]}"#,
            &mut acc,
            &mut out,
        );
        // Argument fragment, no finish.
        process_chunk(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"msg\":\"hi\"}"}}]}}]}"#,
            &mut acc,
            &mut out,
        );
        // Terminal chunk: finish_reason tool_calls, empty delta.
        process_chunk(
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            &mut acc,
            &mut out,
        );
        let done = out.into_iter().map(|r| r.unwrap()).next_back().unwrap();
        match done {
            OpenRouterChunk::Done(FinishReason::ToolUse(calls)) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "call_9");
                assert_eq!(calls[0].name, "echo");
                assert_eq!(calls[0].arguments, serde_json::json!({"msg": "hi"}));
            }
            other => panic!("expected Done(ToolUse), got {other:?}"),
        }
    }

    #[test]
    fn finish_reason_stop_maps_to_done_stop() {
        let events = events_of(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#);
        assert_eq!(events, vec![OpenRouterChunk::Done(FinishReason::Stop)]);
    }

    #[test]
    fn usage_chunk_maps_to_usage_event() {
        let events =
            events_of(r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":4}}"#);
        assert_eq!(
            events,
            vec![OpenRouterChunk::Usage(Usage {
                tokens_in: 11,
                tokens_out: 4,
                cost_cents: None,
            })]
        );
    }

    #[test]
    fn malformed_chunk_becomes_upstream_error() {
        let mut acc = ToolCallAccumulator::new();
        let mut out = VecDeque::new();
        process_chunk("{not json", &mut acc, &mut out);
        assert!(matches!(
            out.pop_front().unwrap(),
            Err(ProviderError::Upstream(_))
        ));
    }

    /// Drive a sequence of [`OpenRouterChunk`]s through an [`EventAdapter`] and
    /// collect the shared events it emits.
    fn adapt_all(chunks: Vec<OpenRouterChunk>) -> Vec<StreamEvent> {
        let mut adapter = EventAdapter::default();
        let mut out = VecDeque::new();
        for c in chunks {
            adapter.adapt(c, &mut out);
        }
        out.into_iter().map(|r| r.unwrap()).collect()
    }

    fn td(index: u32, id: Option<&str>, name: Option<&str>, args: &str) -> OpenRouterChunk {
        OpenRouterChunk::ToolCall(ToolCallDelta {
            index,
            id: id.map(str::to_string),
            name: name.map(str::to_string),
            arguments: args.to_string(),
        })
    }

    #[test]
    fn adapter_maps_content_usage_and_finish() {
        let events = adapt_all(vec![
            OpenRouterChunk::Content("hi".to_string()),
            OpenRouterChunk::Usage(Usage {
                tokens_in: 3,
                tokens_out: 1,
                cost_cents: None,
            }),
            OpenRouterChunk::Done(FinishReason::Stop),
        ]);
        assert_eq!(
            events,
            vec![
                StreamEvent::ContentDelta("hi".to_string()),
                StreamEvent::Usage(Usage {
                    tokens_in: 3,
                    tokens_out: 1,
                    cost_cents: None,
                }),
                StreamEvent::Finish(FinishReason::Stop),
            ]
        );
    }

    #[test]
    fn adapter_tool_call_id_first() {
        // id + name on the opening fragment, arguments streamed after.
        let events = adapt_all(vec![
            td(0, Some("call_1"), Some("echo"), ""),
            td(0, None, None, r#"{"msg":"#),
            td(0, None, None, r#""hi"}"#),
        ]);
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolCallStart(ToolCall {
                    id: "call_1".to_string(),
                    name: "echo".to_string(),
                    arguments: serde_json::Value::Null,
                }),
                StreamEvent::ToolCallDelta {
                    id: "call_1".to_string(),
                    delta: r#"{"msg":"#.to_string(),
                },
                StreamEvent::ToolCallDelta {
                    id: "call_1".to_string(),
                    delta: r#""hi"}"#.to_string(),
                },
            ]
        );
    }

    #[test]
    fn adapter_tool_call_id_late_buffers() {
        // arguments arrive before the id fragment; they are buffered, then the
        // start is emitted and the buffered fragments flush — all keyed by the id.
        let events = adapt_all(vec![
            td(0, None, None, r#"{"msg":"#),
            td(0, None, None, r#""hi"}"#),
            td(0, Some("call_9"), Some("echo"), ""),
        ]);
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolCallStart(ToolCall {
                    id: "call_9".to_string(),
                    name: "echo".to_string(),
                    arguments: serde_json::Value::Null,
                }),
                StreamEvent::ToolCallDelta {
                    id: "call_9".to_string(),
                    delta: r#"{"msg":"#.to_string(),
                },
                StreamEvent::ToolCallDelta {
                    id: "call_9".to_string(),
                    delta: r#""hi"}"#.to_string(),
                },
            ]
        );
    }

    #[test]
    fn adapter_multi_index_tool_calls() {
        // Two parallel calls at index 0 and 1, interleaved.
        let events = adapt_all(vec![
            td(0, Some("a"), Some("first"), ""),
            td(1, Some("b"), Some("second"), ""),
            td(0, None, None, "{}"),
            td(1, None, None, "[]"),
        ]);
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolCallStart(ToolCall {
                    id: "a".to_string(),
                    name: "first".to_string(),
                    arguments: serde_json::Value::Null,
                }),
                StreamEvent::ToolCallStart(ToolCall {
                    id: "b".to_string(),
                    name: "second".to_string(),
                    arguments: serde_json::Value::Null,
                }),
                StreamEvent::ToolCallDelta {
                    id: "a".to_string(),
                    delta: "{}".to_string(),
                },
                StreamEvent::ToolCallDelta {
                    id: "b".to_string(),
                    delta: "[]".to_string(),
                },
            ]
        );
    }
}
