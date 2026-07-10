//! Progressive streaming display for a chat turn (§2.1b + §2.X polish).
//!
//! [`drive_fused_turn`] is the default CLI renderer. It consumes
//! [`FusedEvent`] values from the full ten-stage runtime pipeline, so progressive
//! output keeps capability, Cedar, budget, receipt, memory, and durable-journal
//! guarantees. [`drive_turn`] remains the provider-level compatibility renderer
//! used by focused rendering tests and non-fused callers.
//!
//! Both surfaces stream content deltas live (raw, for responsiveness), draw
//! themed tool boxes, and close with token/cost information. Rendering flows
//! through a [`RenderCtx`] (theme, width, and OSC-8 capability), so colour and
//! `NO_COLOR`/`--plain` behavior remain centralized.

use std::collections::HashMap;
use std::io::Write;
use std::time::{Duration, Instant};

use ardur_fused_runtime::FusedEvent;
use ardur_provider_runtime::{
    CompletionRequest, FinishReason, Provider, RateCard, StreamEvent, Usage,
};
use ardur_runtime::{ReceiptId, RuntimeError};
use futures::{Stream, StreamExt as _};

use crate::anim::{CLEAR_LINE, TYPING_DOTS_TICK, TypingDots};
use crate::markdown::render_markdown_with;
use crate::theme::{Role, Theme};
use crate::toolbox::{TurnStats, render_cost_line, render_tool_call_box};

/// The presentation context every turn renders through: the active theme, the
/// column budget boxes/tables lay out against, and whether OSC-8 hyperlinks are
/// emitted. Built once per turn by the REPL (or with a fixed width in tests).
#[derive(Clone, Debug)]
pub struct RenderCtx<'a> {
    /// The active theme (carries the `NO_COLOR`/plain flag).
    pub theme: &'a Theme,
    /// The layout width in columns.
    pub width: usize,
    /// Whether to emit OSC-8 hyperlinks for Markdown links.
    pub osc8: bool,
}

impl<'a> RenderCtx<'a> {
    /// A context for `theme` at `width` with OSC-8 off.
    #[must_use]
    pub fn new(theme: &'a Theme, width: usize) -> Self {
        Self {
            theme,
            width,
            osc8: false,
        }
    }
}

/// The accumulated result of rendering one turn — the assembled text (so the REPL
/// can append it to history), the final token ledger, why generation stopped, the
/// names of any tool calls the model requested, and any error that aborted the
/// stream (the partial output was still shown).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamOutcome {
    /// The full assistant text, concatenated from every content delta for display.
    pub content: String,
    /// Per-round assistant responses that crossed the receipt commit boundary, in
    /// journal order. Live history uses these instead of collapsing tool loops.
    pub committed_assistant_messages: Vec<String>,
    /// The final token ledger, when the turn reported usage.
    pub usage: Option<Usage>,
    /// The turn's committed provider-plus-tool cost in US cents, when at least
    /// one receipt was minted — fed into the session `/cost` tally.
    pub cost_cents: Option<u64>,
    /// Receipt ids committed by completed fused rounds, in event order. A
    /// non-empty vector means durable history exists even if a later stage failed.
    pub receipt_ids: Vec<ReceiptId>,
    /// The terminal finish reason, when the turn finished cleanly.
    pub finish_reason: Option<FinishReason>,
    /// Names of the tools the model requested this turn (in arrival order).
    pub tool_calls: Vec<String>,
    /// The error that aborted the turn, if any — the partial `content` above was
    /// still rendered before this was set.
    pub error: Option<String>,
}

/// Drive one chat turn through `provider`, rendering it to `out` via `ctx`.
///
/// When `stream_enabled` and the provider [`supports_streaming`], this consumes
/// [`Provider::stream`] and renders events progressively; otherwise it runs a
/// single [`Provider::complete`] and renders the finished response (Markdown) through
/// the same surface. A failure to *start* the stream, or a `complete()` failure,
/// is printed and returned on [`StreamOutcome::error`]; a *mid-stream* failure
/// keeps the partial output and appends the error.
///
/// [`supports_streaming`]: Provider::supports_streaming
pub async fn drive_turn<W: Write>(
    provider: &dyn Provider,
    req: CompletionRequest,
    stream_enabled: bool,
    out: &mut W,
    ctx: &RenderCtx<'_>,
) -> std::io::Result<StreamOutcome> {
    let started = Instant::now();
    if stream_enabled && provider.supports_streaming() {
        match provider.stream(req).await {
            Ok(stream) => render_stream(stream, provider.rate_card(), out, ctx, started).await,
            Err(e) => {
                // Failure to *start* the stream: nothing was emitted, so there is
                // no partial output — report and stop.
                let msg = e.to_string();
                write_error(out, &msg, ctx.theme)?;
                Ok(StreamOutcome {
                    error: Some(msg),
                    ..StreamOutcome::default()
                })
            }
        }
    } else {
        render_complete(provider, req, out, ctx, started).await
    }
}

/// Render one stream from [`ardur_fused_runtime::FusedRuntime::stream`].
///
/// Unlike [`drive_turn`]'s provider-level compatibility surface, this consumes
/// events from the full fused pipeline, so a clean streamed turn retains Cedar,
/// cost-gate, receipt, memory, and durable-journal semantics.
pub async fn drive_fused_turn<S, W>(
    stream: S,
    out: &mut W,
    ctx: &RenderCtx<'_>,
) -> std::io::Result<StreamOutcome>
where
    S: Stream<Item = Result<FusedEvent, RuntimeError>>,
    W: Write,
{
    let started = Instant::now();
    futures::pin_mut!(stream);
    let mut outcome = StreamOutcome::default();
    let mut current_round_content = String::new();
    let mut total_cost_cents = 0_u64;
    let mut newline_pending = false;
    let mut pending_tools: HashMap<String, (String, String)> = HashMap::new();

    let mut waiting = ctx.theme.is_styled();
    let mut dots = TypingDots::new();
    let mut ticker = tokio::time::interval(TYPING_DOTS_TICK);

    loop {
        let item = if waiting {
            tokio::select! {
                biased;
                maybe = stream.next() => maybe,
                _ = ticker.tick() => {
                    write!(out, "{CLEAR_LINE}{}", dots.render(ctx.theme))?;
                    out.flush()?;
                    dots.tick();
                    continue;
                }
            }
        } else {
            stream.next().await
        };
        let Some(item) = item else { break };

        let visible = matches!(
            &item,
            Ok(FusedEvent::Content(_) | FusedEvent::ToolCallResult { .. } | FusedEvent::Finish(_))
                | Err(_)
        );
        if waiting && visible {
            write!(out, "{CLEAR_LINE}")?;
            out.flush()?;
            waiting = false;
        }

        match item {
            Ok(FusedEvent::StageStart { .. } | FusedEvent::StageEnd { .. }) => {}
            Ok(FusedEvent::Receipt {
                receipt_id,
                cost_cents,
                ..
            }) => {
                outcome
                    .committed_assistant_messages
                    .push(std::mem::take(&mut current_round_content));
                outcome.receipt_ids.push(receipt_id);
                total_cost_cents = total_cost_cents.saturating_add(cost_cents);
            }
            Ok(FusedEvent::Content(text)) => {
                newline_pending = !text.is_empty() && !text.ends_with('\n');
                current_round_content.push_str(&text);
                outcome.content.push_str(&text);
                write!(out, "{text}")?;
                out.flush()?;
            }
            Ok(FusedEvent::ToolCallStart { id, name }) => {
                outcome.tool_calls.push(name.clone());
                pending_tools.insert(id, (name, String::new()));
            }
            Ok(FusedEvent::ToolCallDelta { id, delta }) => {
                if let Some((_, arguments)) = pending_tools.get_mut(&id) {
                    arguments.push_str(&delta);
                }
            }
            Ok(FusedEvent::ToolCallResult { id, .. }) => {
                if newline_pending {
                    writeln!(out)?;
                    newline_pending = false;
                }
                if let Some((name, arguments)) = pending_tools.remove(&id) {
                    let arguments = serde_json::from_str::<serde_json::Value>(&arguments)
                        .ok()
                        .and_then(|value| serde_json::to_string_pretty(&value).ok())
                        .unwrap_or(arguments);
                    writeln!(
                        out,
                        "{}",
                        render_tool_call_box(&name, &arguments, ctx.theme, ctx.width)
                    )?;
                }
            }
            Ok(FusedEvent::Usage(usage)) => match &mut outcome.usage {
                Some(total) => {
                    total.tokens_in = total.tokens_in.saturating_add(usage.tokens_in);
                    total.tokens_out = total.tokens_out.saturating_add(usage.tokens_out);
                    total.cost_cents = match (total.cost_cents, usage.cost_cents) {
                        (Some(lhs), Some(rhs)) => Some(lhs.saturating_add(rhs)),
                        _ => None,
                    };
                }
                None => outcome.usage = Some(usage),
            },
            Ok(FusedEvent::Finish(reason)) => {
                if newline_pending {
                    writeln!(out)?;
                    newline_pending = false;
                }
                write_finish_note(out, &reason, ctx.theme)?;
                outcome.finish_reason = Some(reason);
            }
            Err(error) => {
                if newline_pending {
                    writeln!(out)?;
                    newline_pending = false;
                }
                let message = error.to_string();
                write_error(out, &message, ctx.theme)?;
                outcome.error = Some(message);
                break;
            }
        }
    }

    if newline_pending {
        writeln!(out)?;
    }
    if !outcome.receipt_ids.is_empty() {
        outcome.cost_cents = Some(total_cost_cents);
    }
    if let Some(usage) = outcome.usage {
        // A pre-commit failure released its reservation, so a token-bearing
        // partial stream with no receipt correctly displays a zero committed cost.
        write_cost_line_with_cents(out, usage, total_cost_cents, started.elapsed(), ctx)?;
    }
    Ok(outcome)
}

/// Render a live [`ProviderStream`](ardur_provider_runtime::ProviderStream),
/// event by event, flushing content as it arrives.
async fn render_stream<W: Write>(
    mut stream: ardur_provider_runtime::ProviderStream,
    rate_card: &RateCard,
    out: &mut W,
    ctx: &RenderCtx<'_>,
    started: Instant,
) -> std::io::Result<StreamOutcome> {
    let mut outcome = StreamOutcome::default();
    // Whether the last thing written was content not ending in a newline, so the
    // next structural line (tool box / finish / error / cost) starts fresh.
    let mut newline_pending = false;

    // Typing dots: while awaiting the first event, pulse `·`/`··`/`···` at 4 Hz on
    // a single line, erased the instant anything arrives. Only when styling is on
    // (a tty, not `NO_COLOR`/`--plain`) — so piped/test output stays clean.
    let mut waiting = ctx.theme.is_styled();
    let mut dots = TypingDots::new();
    let mut ticker = tokio::time::interval(TYPING_DOTS_TICK);

    loop {
        let item = if waiting {
            tokio::select! {
                biased;
                maybe = stream.next() => maybe,
                _ = ticker.tick() => {
                    write!(out, "{CLEAR_LINE}{}", dots.render(ctx.theme))?;
                    out.flush()?;
                    dots.tick();
                    continue;
                }
            }
        } else {
            stream.next().await
        };
        let Some(item) = item else { break };
        if waiting {
            // First event arrived — erase the dots line and render normally.
            write!(out, "{CLEAR_LINE}")?;
            out.flush()?;
            waiting = false;
        }
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
                let args = serde_json::to_string_pretty(&call.arguments).unwrap_or_default();
                writeln!(
                    out,
                    "{}",
                    render_tool_call_box(&call.name, &args, ctx.theme, ctx.width)
                )?;
                outcome.tool_calls.push(call.name);
            }
            // Argument fragments don't affect the display; the assembled call is
            // already drawn by its `ToolCallStart` box.
            Ok(StreamEvent::ToolCallDelta { .. }) => {}
            Ok(StreamEvent::Usage(usage)) => outcome.usage = Some(usage),
            Ok(StreamEvent::Finish(reason)) => {
                if newline_pending {
                    writeln!(out)?;
                    newline_pending = false;
                }
                write_finish_note(out, &reason, ctx.theme)?;
                outcome.finish_reason = Some(reason);
            }
            Err(e) => {
                if newline_pending {
                    writeln!(out)?;
                    newline_pending = false;
                }
                let msg = e.to_string();
                write_error(out, &msg, ctx.theme)?;
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
        outcome.cost_cents = Some(rate_card.price(usage).cents);
        write_cost_line(out, usage, rate_card, started.elapsed(), ctx)?;
    }
    Ok(outcome)
}

/// Render a single non-streaming [`Provider::complete`] response: the content
/// through the full Markdown renderer, any tool-call boxes, then the cost line.
async fn render_complete<W: Write>(
    provider: &dyn Provider,
    req: CompletionRequest,
    out: &mut W,
    ctx: &RenderCtx<'_>,
    started: Instant,
) -> std::io::Result<StreamOutcome> {
    match provider.complete(req).await {
        Ok(resp) => {
            let rendered = render_markdown_with(&resp.content, ctx.theme, ctx.width, ctx.osc8);
            writeln!(out, "{rendered}")?;
            let mut tool_calls = Vec::new();
            if let FinishReason::ToolUse(calls) = &resp.finish_reason {
                for call in calls {
                    let args = serde_json::to_string_pretty(&call.arguments).unwrap_or_default();
                    writeln!(
                        out,
                        "{}",
                        render_tool_call_box(&call.name, &args, ctx.theme, ctx.width)
                    )?;
                    tool_calls.push(call.name.clone());
                }
            }
            write_finish_note(out, &resp.finish_reason, ctx.theme)?;
            write_cost_line(
                out,
                resp.usage,
                provider.rate_card(),
                started.elapsed(),
                ctx,
            )?;
            Ok(StreamOutcome {
                content: resp.content,
                committed_assistant_messages: Vec::new(),
                usage: Some(resp.usage),
                cost_cents: Some(provider.rate_card().price(resp.usage).cents),
                receipt_ids: Vec::new(),
                finish_reason: Some(resp.finish_reason),
                tool_calls,
                error: None,
            })
        }
        Err(e) => {
            let msg = e.to_string();
            write_error(out, &msg, ctx.theme)?;
            Ok(StreamOutcome {
                error: Some(msg),
                ..StreamOutcome::default()
            })
        }
    }
}

/// Write the themed, context-pressure-coloured end-of-turn cost line.
fn write_cost_line<W: Write>(
    out: &mut W,
    usage: Usage,
    rate_card: &RateCard,
    elapsed: Duration,
    ctx: &RenderCtx<'_>,
) -> std::io::Result<()> {
    write_cost_line_with_cents(out, usage, rate_card.price(usage).cents, elapsed, ctx)
}

fn write_cost_line_with_cents<W: Write>(
    out: &mut W,
    usage: Usage,
    cost_cents: u64,
    elapsed: Duration,
    ctx: &RenderCtx<'_>,
) -> std::io::Result<()> {
    let dollars = cost_cents as f64 / 100.0;
    let stats = TurnStats {
        tokens_in: u64::from(usage.tokens_in),
        tokens_out: u64::from(usage.tokens_out),
        cost_dollars: dollars,
        elapsed,
        context_frac: None,
    };
    writeln!(out, "{}", render_cost_line(&stats, ctx.theme, ctx.width))
}

/// Note a non-default finish reason in dim, e.g. `[finish: max tokens]`. A clean
/// `Stop`, a `ToolUse` (already surfaced by its boxes), and an `Error` (surfaced
/// by [`write_error`]) print nothing here.
fn write_finish_note<W: Write>(
    out: &mut W,
    reason: &FinishReason,
    theme: &Theme,
) -> std::io::Result<()> {
    let note = match reason {
        FinishReason::MaxTokens => "[finish: max tokens]".to_string(),
        FinishReason::StopSequence(s) => format!("[finish: stop sequence {s:?}]"),
        FinishReason::Stop | FinishReason::ToolUse(_) | FinishReason::Error(_) => return Ok(()),
    };
    writeln!(out, "{}", theme.paint(Role::Dim, &note))
}

/// Write a dim `error: <msg>` line.
fn write_error<W: Write>(out: &mut W, msg: &str, theme: &Theme) -> std::io::Result<()> {
    writeln!(
        out,
        "{}",
        theme.paint(Role::Error, &format!("error: {msg}"))
    )
}
