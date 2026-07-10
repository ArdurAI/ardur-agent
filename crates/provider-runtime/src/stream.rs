//! The streaming surface of the [`Provider`](crate::Provider) trait (§3.1b).
//!
//! [`StreamEvent`] is the incremental unit a streaming completion delivers, and
//! [`ProviderStream`] is the boxed, object-safe stream type
//! [`Provider::stream`](crate::Provider::stream) returns. Both reuse the
//! existing value types — [`ToolCall`], [`FinishReason`], [`Usage`] — so a
//! streamed turn and a one-shot [`complete`](crate::Provider::complete) speak
//! the same vocabulary.

use std::pin::Pin;

use ardur_runtime::ToolCall;
use futures::Stream;

use crate::error::ProviderError;
use crate::types::{CompletionResponse, FinishReason, Usage};

/// One incremental event from a streaming completion.
///
/// A stream yields these in arrival order. Text arrives as a run of
/// [`ContentDelta`](StreamEvent::ContentDelta); a model-requested tool call
/// arrives as one [`ToolCallStart`](StreamEvent::ToolCallStart) (carrying the
/// call id and name) followed by zero or more
/// [`ToolCallDelta`](StreamEvent::ToolCallDelta) fragments that concatenate into
/// the call's JSON arguments. [`Usage`](StreamEvent::Usage) carries the running
/// token ledger (emitted early with input tokens, and again with the final
/// totals), and [`Finish`](StreamEvent::Finish) is the terminal event naming why
/// generation stopped — with any fully-assembled tool calls on
/// [`FinishReason::ToolUse`].
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    /// A chunk of generated text to append to the response so far.
    ContentDelta(String),
    /// The model began a tool call: the id and name are known, the arguments
    /// follow as [`ToolCallDelta`](StreamEvent::ToolCallDelta) fragments. The
    /// carried [`ToolCall`] has empty (`null`) `arguments` at this point.
    ToolCallStart(ToolCall),
    /// A fragment of a tool call's JSON `arguments`, keyed to the call `id` the
    /// preceding [`ToolCallStart`](StreamEvent::ToolCallStart) announced.
    ToolCallDelta {
        /// The id of the tool call this fragment belongs to.
        id: String,
        /// A partial-JSON fragment to append to that call's arguments buffer.
        delta: String,
    },
    /// The token ledger so far. Emitted at least twice on a live stream: once
    /// near the start (input tokens, output zero) and once at the end with the
    /// final input+output totals the receipt is minted from.
    Usage(Usage),
    /// Generation finished; carries the terminal [`FinishReason`] (with any
    /// assembled tool calls). This is the last event a well-formed stream emits.
    Finish(FinishReason),
    /// The actual model name the provider served (when different from the
    /// requested model — e.g. a fallback or version-resolved alias). Emitted
    /// early in the stream by providers that include it, so the observability
    /// layer can record `gen_ai.response.model` faithfully rather than
    /// echoing the requested model.
    ServedModel(String),
}

/// The stream type [`Provider::stream`](crate::Provider::stream) returns.
///
/// Boxed and pinned so the method stays object-safe on `dyn Provider`; `Send` so
/// the stream can be polled across the async runtime's threads. Each item is a
/// [`StreamEvent`] or the [`ProviderError`] that aborted the stream.
pub type ProviderStream =
    Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send + 'static>>;

/// Box a ready vector of events into a [`ProviderStream`] — the building block
/// both the trait's default `stream()` and the stub backend hand back.
pub(crate) fn iter_events(events: Vec<StreamEvent>) -> ProviderStream {
    Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
}

/// Flatten a finished [`CompletionResponse`] into the event sequence a single
/// non-streaming call would have produced incrementally: the content as one
/// [`ContentDelta`](StreamEvent::ContentDelta) (when non-empty), a
/// [`ToolCallStart`](StreamEvent::ToolCallStart) per requested call, the final
/// [`Usage`](StreamEvent::Usage), then the terminal
/// [`Finish`](StreamEvent::Finish).
///
/// This is what the [`Provider::stream`](crate::Provider::stream) default impl
/// uses to bridge a provider that only implements `complete()` — non-breaking
/// for every backend that does not override `stream()`.
pub(crate) fn events_from_response(resp: CompletionResponse) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    if !resp.content.is_empty() {
        events.push(StreamEvent::ContentDelta(resp.content.clone()));
    }
    if let FinishReason::ToolUse(calls) = &resp.finish_reason {
        for call in calls {
            events.push(StreamEvent::ToolCallStart(call.clone()));
        }
    }
    events.push(StreamEvent::Usage(resp.usage));
    events.push(StreamEvent::Finish(resp.finish_reason));
    events
}
