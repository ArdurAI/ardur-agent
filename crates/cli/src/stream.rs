//! Progressive streaming display for a chat turn (§2.1b).
//!
//! [`drive_turn`] is the single entry the REPL drives a provider through: when
//! streaming is enabled and the backend [`supports_streaming`], it consumes
//! [`Provider::stream`] and renders each [`StreamEvent`] as it arrives —
//! [`ContentDelta`](StreamEvent::ContentDelta)s print and flush immediately, a
//! [`ToolCallStart`](StreamEvent::ToolCallStart) shows a `→ calling tool:`
//! indicator, [`Usage`](StreamEvent::Usage) ends the turn with a dim token/cost
//! line, and a mid-stream error prints the partial output already shown plus the
//! error rather than crashing. Otherwise it falls back to a single
//! [`Provider::complete`] call, rendered through the same surface.
//!
//! The rendering is generic over any [`Write`] sink (stdout in the REPL, a buffer
//! in tests) and a `color` flag toggling the raw ANSI escapes the CLI already
//! uses for its prompt — so it adds no new color dependency.
//!
//! # Trade-off (documented)
//!
//! The streamed path calls [`Provider::stream`] **directly at the CLI layer**,
//! bypassing the [`FusedRuntime`](ardur_fused_runtime::FusedRuntime)'s ten-stage
//! pipeline (cap-token verify, Cedar authorization, cost admission, signed
//! receipt, durable journal) that the non-streaming [`complete`](Provider::complete)
//! path routes through. This is the minimal §2.1b wiring; threading streaming
//! through the fused runtime is the proposed follow-up.
//!
//! [`Provider::stream`]: ardur_provider_runtime::Provider::stream
//! [`Provider::complete`]: ardur_provider_runtime::Provider::complete
//! [`supports_streaming`]: ardur_provider_runtime::Provider::supports_streaming

use std::io::Write;

use ardur_provider_runtime::{
    CompletionRequest, FinishReason, Provider, RateCard, StreamEvent, Usage,
};
use futures::StreamExt as _;

/// ANSI escapes matching the CLI's existing raw-escape styling (see the prompt in
/// `lib.rs`). `YELLOW` flags a tool-call indicator; `DIM` greys the cost line and
/// any error/finish note; `RESET` clears.
const YELLOW: &str = "\x1b[33m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// The accumulated result of rendering one turn — the assembled text (so the REPL
/// can append it to history), the final token ledger, why generation stopped, the
/// names of any tool calls the model requested, and any error that aborted the
/// stream (the partial output was still shown).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamOutcome {
    /// The full assistant text, concatenated from every content delta.
    pub content: String,
    /// The final token ledger, when the turn reported usage.
    pub usage: Option<Usage>,
    /// The terminal finish reason, when the turn finished cleanly.
    pub finish_reason: Option<FinishReason>,
    /// Names of the tools the model requested this turn (in arrival order).
    pub tool_calls: Vec<String>,
    /// The error that aborted the turn, if any — the partial `content` above was
    /// still rendered before this was set.
    pub error: Option<String>,
}

/// Drive one chat turn through `provider`, rendering it to `out`.
///
/// When `stream_enabled` and the provider [`supports_streaming`], this consumes
/// [`Provider::stream`] and renders events progressively; otherwise it runs a
/// single [`Provider::complete`] and renders the finished response through the
/// same surface. A failure to *start* the stream, or a `complete()` failure, is
/// printed (no partial output exists) and returned on
/// [`StreamOutcome::error`]; a *mid-stream* failure keeps the partial output and
/// appends the error.
///
/// [`supports_streaming`]: Provider::supports_streaming
pub async fn drive_turn<W: Write>(
    provider: &dyn Provider,
    req: CompletionRequest,
    stream_enabled: bool,
    out: &mut W,
    color: bool,
) -> std::io::Result<StreamOutcome> {
    if stream_enabled && provider.supports_streaming() {
        match provider.stream(req).await {
            Ok(stream) => render_stream(stream, provider.rate_card(), out, color).await,
            Err(e) => {
                // Failure to *start* the stream: nothing was emitted, so there is
                // no partial output — report and stop.
                let msg = e.to_string();
                write_error(out, &msg, color)?;
                Ok(StreamOutcome {
                    error: Some(msg),
                    ..StreamOutcome::default()
                })
            }
        }
    } else {
        render_complete(provider, req, out, color).await
    }
}

/// Render a live [`ProviderStream`](ardur_provider_runtime::ProviderStream),
/// event by event, flushing content as it arrives.
async fn render_stream<W: Write>(
    mut stream: ardur_provider_runtime::ProviderStream,
    rate_card: &RateCard,
    out: &mut W,
    color: bool,
) -> std::io::Result<StreamOutcome> {
    let mut outcome = StreamOutcome::default();
    // Whether the last thing written was content not ending in a newline, so the
    // next structural line (tool indicator / finish / error / cost) starts fresh.
    let mut newline_pending = false;

    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamEvent::ContentDelta(text)) => {
                newline_pending = !text.is_empty() && !text.ends_with('\n');
                outcome.content.push_str(&text);
                write!(out, "{text}")?;
                out.flush()?;
            }
            Ok(StreamEvent::ToolCallStart(call)) => {
                if newline_pending {
                    writeln!(out)?;
                    newline_pending = false;
                }
                write_tool_indicator(out, &call.name, color)?;
                outcome.tool_calls.push(call.name);
            }
            // Argument fragments don't affect the display; the assembled call is
            // already announced by its `ToolCallStart`.
            Ok(StreamEvent::ToolCallDelta { .. }) => {}
            Ok(StreamEvent::Usage(usage)) => outcome.usage = Some(usage),
            Ok(StreamEvent::Finish(reason)) => {
                if newline_pending {
                    writeln!(out)?;
                    newline_pending = false;
                }
                write_finish_note(out, &reason, color)?;
                outcome.finish_reason = Some(reason);
            }
            Err(e) => {
                if newline_pending {
                    writeln!(out)?;
                    newline_pending = false;
                }
                let msg = e.to_string();
                write_error(out, &msg, color)?;
                outcome.error = Some(msg);
                // Partial output is preserved; stop consuming the aborted stream.
                break;
            }
        }
    }

    // A stream that ended without a `Finish` (or errored mid-content) may still
    // owe a trailing newline so the cost line and next prompt start clean.
    if newline_pending {
        writeln!(out)?;
    }
    if let Some(usage) = outcome.usage {
        write_usage_line(out, usage, rate_card, color)?;
    }
    Ok(outcome)
}

/// Render a single non-streaming [`Provider::complete`] response through the same
/// surface a stream would produce: the content, any tool-call indicators, then
/// the dim cost line.
async fn render_complete<W: Write>(
    provider: &dyn Provider,
    req: CompletionRequest,
    out: &mut W,
    color: bool,
) -> std::io::Result<StreamOutcome> {
    match provider.complete(req).await {
        Ok(resp) => {
            write!(out, "{}", resp.content)?;
            if !resp.content.ends_with('\n') {
                writeln!(out)?;
            }
            let mut tool_calls = Vec::new();
            if let FinishReason::ToolUse(calls) = &resp.finish_reason {
                for call in calls {
                    write_tool_indicator(out, &call.name, color)?;
                    tool_calls.push(call.name.clone());
                }
            }
            write_finish_note(out, &resp.finish_reason, color)?;
            write_usage_line(out, resp.usage, provider.rate_card(), color)?;
            Ok(StreamOutcome {
                content: resp.content,
                usage: Some(resp.usage),
                finish_reason: Some(resp.finish_reason),
                tool_calls,
                error: None,
            })
        }
        Err(e) => {
            let msg = e.to_string();
            write_error(out, &msg, color)?;
            Ok(StreamOutcome {
                error: Some(msg),
                ..StreamOutcome::default()
            })
        }
    }
}

/// Write the `→ calling tool: <name>` indicator on its own line (yellow when
/// `color`).
fn write_tool_indicator<W: Write>(out: &mut W, name: &str, color: bool) -> std::io::Result<()> {
    if color {
        writeln!(out, "{YELLOW}→ calling tool: {name}{RESET}")
    } else {
        writeln!(out, "→ calling tool: {name}")
    }
}

/// Write the dim end-of-turn token/cost line, e.g.
/// `[tokens in/out: 12/34, cost: $0.0510]`.
fn write_usage_line<W: Write>(
    out: &mut W,
    usage: Usage,
    rate_card: &RateCard,
    color: bool,
) -> std::io::Result<()> {
    let dollars = rate_card.price(usage).cents as f64 / 100.0;
    let line = format!(
        "[tokens in/out: {}/{}, cost: ${dollars:.4}]",
        usage.tokens_in, usage.tokens_out
    );
    if color {
        writeln!(out, "{DIM}{line}{RESET}")
    } else {
        writeln!(out, "{line}")
    }
}

/// Note a non-default finish reason in dim, e.g. `[finish: max tokens]`. A clean
/// `Stop`, a `ToolUse` (already surfaced by its indicators), and an `Error`
/// (surfaced by [`write_error`]) print nothing here.
fn write_finish_note<W: Write>(
    out: &mut W,
    reason: &FinishReason,
    color: bool,
) -> std::io::Result<()> {
    let note = match reason {
        FinishReason::MaxTokens => "[finish: max tokens]".to_string(),
        FinishReason::StopSequence(s) => format!("[finish: stop sequence {s:?}]"),
        FinishReason::Stop | FinishReason::ToolUse(_) | FinishReason::Error(_) => return Ok(()),
    };
    if color {
        writeln!(out, "{DIM}{note}{RESET}")
    } else {
        writeln!(out, "{note}")
    }
}

/// Write a dim `error: <msg>` line.
fn write_error<W: Write>(out: &mut W, msg: &str, color: bool) -> std::io::Result<()> {
    if color {
        writeln!(out, "{DIM}error: {msg}{RESET}")
    } else {
        writeln!(out, "error: {msg}")
    }
}
