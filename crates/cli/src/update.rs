//! UI-independent semantics for a fused turn. Both REPL and future TUI consumers
//! use [`UpdateStream`] (or [`TurnReducer`] for synchronous replay); neither needs
//! to rebuild tool arguments, committed round history, or receipt accounting.
//!
//! One source event produces one update. Reduction is synchronous, with no task,
//! channel, clock, rendering, or I/O. The adapter polls only on consumer demand;
//! dropping it drops its source normally. Runtime supervision and settlement stay
//! with the owner of that source, not this presentation layer.

use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};

use ardur_fused_runtime::{FusedEvent, StageKind};
use ardur_provider_runtime::{FinishReason, Usage};
use ardur_runtime::{ReceiptId, RuntimeError};
use futures::{Stream, stream::FusedStream};
use serde::{Deserialize, Serialize};

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

/// The three-valued governance contract. Absence of verification is unverified,
/// not success. Pipeline progress, denials, receipts and provider finish reasons
/// are not themselves a verified governance verdict.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Explicit verification established compliance.
    Compliant,
    /// Explicit verification established a violation.
    Violation,
    /// Verification is absent or incomplete; the initial state for consumers.
    #[default]
    InsufficientEvidence,
}

/// One tool call's identity and verbatim, assembled argument fragments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCallInfo {
    /// The registered tool name supplied at call start.
    pub name: String,
    /// Raw arguments, possibly incomplete or non-JSON. Formatting is a UI concern.
    pub arguments: String,
}

/// A semantic change to one streamed turn, independent of terminal rendering.
///
/// Text, tool arguments and diagnostic errors are not sanitized public labels.
/// In particular, future UIs must match typed [`RuntimeError`] variants, not parse
/// or display their free-form reasons. Only the legacy REPL retains its existing
/// diagnostic text. Tool results are the runtime-scanned value, never re-fetched.
#[derive(Debug)]
pub enum Update {
    /// A pipeline stage entered.
    StageStart {
        /// The source stage, unchanged.
        stage: StageKind,
    },
    /// A pipeline stage exited; `ok` is stage progress, not a governance verdict.
    StageEnd {
        /// The source stage, unchanged.
        stage: StageKind,
        /// Whether this stage passed.
        ok: bool,
    },
    /// A verbatim text fragment (including empty fragments).
    ContentDelta(String),
    /// A tool began; a duplicate id replaces its pending argument buffer.
    ToolCallStart {
        /// The provider-assigned call id.
        id: String,
        /// The tool name.
        name: String,
    },
    /// An argument fragment. Unknown ids are forwarded but not accumulated.
    ToolCallDelta {
        /// The provider-assigned call id.
        id: String,
        /// The verbatim fragment.
        delta: String,
    },
    /// A runtime-scanned result, with the shared reducer's assembled call.
    ToolCallResult {
        /// The provider-assigned call id.
        id: String,
        /// None for an unknown id; no call or tool name is invented.
        call: Option<ToolCallInfo>,
        /// The exact runtime-scanned JSON result. The REPL does not print this.
        result: serde_json::Value,
    },
    /// Usage is advisory provider accounting, not the committed receipt cost.
    Usage {
        /// This round's ledger, unchanged.
        usage: Usage,
        /// Saturating sum across observed rounds; a missing cost makes the
        /// aggregate provider cost unknown, never a partial known total.
        total: Usage,
    },
    /// A receipt boundary committed one round (including an empty response).
    ReceiptMinted {
        /// The minted receipt's id.
        receipt_id: ReceiptId,
        /// The source chain tail, unchanged; not a signature-verification claim.
        chain_hash: String,
        /// This receipt's authoritative provider-plus-tool cost, in cents.
        cost_cents: u64,
        /// Saturating sum of authoritative receipt costs for the turn.
        total_cost_cents: u64,
        /// The exact round content committed at this boundary.
        committed_content: String,
    },
    /// Provider finish reason. As on the legacy path, this does not stop polling;
    /// only a source error or EOF terminates consumption.
    Finish(FinishReason),
    /// Terminal failure, retaining approval/denial/cancellation distinctions and
    /// their structured fields. Free-form reasons are diagnostic, not safe public
    /// text and not a governance verdict. No later source item is consumed.
    Error(RuntimeError),
    /// Reserved seam for an explicit verification producer. No current
    /// [`FusedEvent`] supplies verification, so M0 never synthesizes this update.
    /// Consumers start at [`Verdict::InsufficientEvidence`].
    Verdict(Verdict),
}

/// The shared turn history and accounting accumulator. No rendering state lives
/// here; an empty delta is still delivered even though it adds no content.
#[derive(Debug, Default)]
pub struct TurnReducer {
    outcome: StreamOutcome,
    current_round_content: String,
    pending_tools: HashMap<String, ToolCallInfo>,
}

impl TurnReducer {
    /// Consume one source event synchronously. Returns None after the first
    /// error, without changing the outcome. Receipt costs become available at
    /// the receipt boundary, independently of usage and terminal finish events.
    pub fn reduce(&mut self, item: Result<FusedEvent, RuntimeError>) -> Option<Update> {
        if self.is_terminated() {
            return None;
        }
        Some(match item {
            Ok(FusedEvent::StageStart { stage }) => Update::StageStart { stage },
            Ok(FusedEvent::StageEnd { stage, ok }) => Update::StageEnd { stage, ok },
            Ok(FusedEvent::Content(text)) => {
                self.current_round_content.push_str(&text);
                self.outcome.content.push_str(&text);
                Update::ContentDelta(text)
            }
            Ok(FusedEvent::ToolCallStart { id, name }) => {
                self.outcome.tool_calls.push(name.clone());
                self.pending_tools.insert(
                    id.clone(),
                    ToolCallInfo {
                        name: name.clone(),
                        arguments: String::new(),
                    },
                );
                Update::ToolCallStart { id, name }
            }
            Ok(FusedEvent::ToolCallDelta { id, delta }) => {
                if let Some(call) = self.pending_tools.get_mut(&id) {
                    call.arguments.push_str(&delta);
                }
                Update::ToolCallDelta { id, delta }
            }
            Ok(FusedEvent::ToolCallResult { id, result }) => {
                let call = self.pending_tools.remove(&id);
                Update::ToolCallResult { id, call, result }
            }
            Ok(FusedEvent::Usage(usage)) => {
                let total = match self.outcome.usage {
                    Some(total) => Usage {
                        tokens_in: total.tokens_in.saturating_add(usage.tokens_in),
                        tokens_out: total.tokens_out.saturating_add(usage.tokens_out),
                        cost_cents: match (total.cost_cents, usage.cost_cents) {
                            (Some(lhs), Some(rhs)) => Some(lhs.saturating_add(rhs)),
                            _ => None,
                        },
                    },
                    None => usage,
                };
                self.outcome.usage = Some(total);
                Update::Usage { usage, total }
            }
            Ok(FusedEvent::Receipt {
                receipt_id,
                chain_hash,
                cost_cents,
            }) => {
                let committed_content = std::mem::take(&mut self.current_round_content);
                self.outcome
                    .committed_assistant_messages
                    .push(committed_content.clone());
                self.outcome.receipt_ids.push(receipt_id);
                let total_cost_cents = self
                    .outcome
                    .cost_cents
                    .unwrap_or(0)
                    .saturating_add(cost_cents);
                self.outcome.cost_cents = Some(total_cost_cents);
                Update::ReceiptMinted {
                    receipt_id,
                    chain_hash,
                    cost_cents,
                    total_cost_cents,
                    committed_content,
                }
            }
            Ok(FusedEvent::Finish(reason)) => {
                self.outcome.finish_reason = Some(reason.clone());
                Update::Finish(reason)
            }
            Err(error) => {
                self.outcome.error = Some(error.to_string());
                Update::Error(error)
            }
        })
    }

    /// Accumulated semantics, without any rendering or I/O. Uncommitted trailing
    /// text is display-only; durable history uses `committed_assistant_messages`.
    #[must_use]
    pub fn outcome(&self) -> &StreamOutcome {
        &self.outcome
    }

    /// Pending tool buffers, keyed by call id. Iteration order is unspecified;
    /// consumers use start updates for arrival order and never reassemble deltas.
    #[must_use]
    pub fn pending_tools(&self) -> &HashMap<String, ToolCallInfo> {
        &self.pending_tools
    }

    /// Whether a source error has made further reduction a no-op.
    #[must_use]
    pub fn is_terminated(&self) -> bool {
        self.outcome.error.is_some()
    }

    /// Take the accumulated outcome, preserving the REPL's public return type.
    #[must_use]
    pub fn into_outcome(self) -> StreamOutcome {
        self.outcome
    }
}

/// Pull-based adapter over the shared synchronous reducer. It never reads ahead:
/// each poll polls the source at most once. Error and EOF latch termination.
///
/// Pin a non-Unpin source before passing it in (`futures::pin_mut!` and a mutable
/// pinned reference suffice); the adapter allocates no task, channel or queue.
/// Supervision and settlement remain the responsibility of the source's owner.
#[derive(Debug)]
pub struct UpdateStream<S> {
    source: S,
    reducer: TurnReducer,
    ended: bool,
}

impl<S> UpdateStream<S> {
    /// Wrap a fresh turn's source without polling it.
    pub fn new(source: S) -> Self {
        Self {
            source,
            reducer: TurnReducer::default(),
            ended: false,
        }
    }

    /// The live shared state, already reduced before an update is yielded.
    #[must_use]
    pub fn reducer(&self) -> &TurnReducer {
        &self.reducer
    }

    /// Take the accumulated outcome. Drops this adapter's source normally, with
    /// no background draining, cancellation or settlement work of its own.
    #[must_use]
    pub fn into_outcome(self) -> StreamOutcome {
        self.reducer.into_outcome()
    }
}

impl<S> Stream for UpdateStream<S>
where
    S: Stream<Item = Result<FusedEvent, RuntimeError>> + Unpin,
{
    type Item = Update;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.ended || this.reducer.is_terminated() {
            return Poll::Ready(None);
        }
        match Pin::new(&mut this.source).poll_next(cx) {
            Poll::Ready(Some(item)) => Poll::Ready(this.reducer.reduce(item)),
            Poll::Ready(None) => {
                this.ended = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S> FusedStream for UpdateStream<S>
where
    S: Stream<Item = Result<FusedEvent, RuntimeError>> + Unpin,
{
    fn is_terminated(&self) -> bool {
        self.ended || self.reducer.is_terminated()
    }
}
