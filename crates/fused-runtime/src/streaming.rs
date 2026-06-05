//! §6.0c — [`FusedRuntime::stream`], the progressive sibling of
//! [`submit`](crate::FusedRuntime::submit).
//!
//! `submit` runs the ten-stage pipeline (cap-token → Cedar → injection-defense →
//! cost-gate → provider → tool-exec → receipt → finalize → memory → journal) and
//! returns one [`SubmitResult`](ardur_runtime::SubmitResult) at the end.
//! `stream` runs the **same** stages over the **same** helpers but yields a
//! [`FusedEvent`] feed as the turn unfolds: stage transitions, token deltas as
//! the provider emits them, tool-call lifecycle, the minted receipt's chain
//! hash, and the terminal finish. A consumer (the CLI, a channel adapter) gets
//! the §2.1b streaming UX **without** dropping the security/observability
//! substrate the way a direct [`Provider::stream`](ardur_provider_runtime::Provider::stream)
//! call at the CLI layer did (PR #89, the bypass this lane closes).
//!
//! # Event model
//!
//! The stream item type is `Result<FusedEvent, RuntimeError>`. The happy path is
//! a run of `Ok(FusedEvent::…)`; a stage that rejects the turn emits its
//! [`StageEnd { ok: false }`](FusedEvent::StageEnd) and then a terminal
//! `Err(RuntimeError)`, after which the stream ends. This mirrors the existing
//! [`ProviderStream`](ardur_provider_runtime::ProviderStream) convention
//! (`Stream<Item = Result<StreamEvent, ProviderError>>`) rather than inventing a
//! second in-band error channel, so the brief's sketched `FusedEvent::Error`
//! variant is folded into the `Err` arm.
//!
//! The stage events bracket each pipeline stage in order:
//!
//! | order | [`StageKind`] | emitted around |
//! |------|--------------|----------------|
//! | 1 | [`CapTokenVerify`](StageKind::CapTokenVerify) | stage 1 cap-token parse + verify |
//! | 2 | [`CedarCheck`](StageKind::CedarCheck) | stage 2 policy evaluation |
//! | 3 | [`CostGateAdmit`](StageKind::CostGateAdmit) | stage 3' per-round admission |
//! | 4 | [`InjectionScan`](StageKind::InjectionScan) | stage 4.5 outbound + tool-output scan |
//! | 5 | [`ProviderStream`](StageKind::ProviderStream) | stage 5 provider token stream |
//! | 6 | [`ToolExec`](StageKind::ToolExec) | stage 6 per-tool invocation |
//! | 7 | [`ReceiptMint`](StageKind::ReceiptMint) | stage 7 receipt mint + chain |
//! | 8 | [`CostGateFinalize`](StageKind::CostGateFinalize) | stage 8 reservation settle |
//! | 9 | [`MemoryRecord`](StageKind::MemoryRecord) | stage 9 bi-temporal record |
//! | 10 | [`JournalAppend`](StageKind::JournalAppend) | stage 10 durable journal append |
//!
//! In a tool-using turn stages 3–10 repeat once per provider round, exactly as
//! `submit`'s loop does.
//!
//! # Cancellation
//!
//! The whole pipeline runs **inside** the returned stream's generator future, so
//! dropping the stream drops that future: the in-flight
//! [`Provider::stream`](ardur_provider_runtime::Provider::stream) is cancelled at
//! the next `.await`, and because the receipt is minted only **after** the
//! provider round completes (stage 7), a turn cancelled mid-generation mints **no
//! receipt**, appends **no** journal entry, and writes **no** memory record — the
//! held cost reservation simply lapses on the gate's TTL. This is the deliberate
//! "a cancelled turn leaves no durable trace" contract: a partial, never-finished
//! response must not enter the auditable chain. (A consumer that needs the
//! partial text still has every [`Content`](FusedEvent::Content) delta it
//! observed before the drop.)

use ardur_provider_runtime::{FinishReason, Usage};
use ardur_runtime::ReceiptId;

/// Which pipeline stage a [`FusedEvent::StageStart`] / [`FusedEvent::StageEnd`]
/// brackets. The numbering follows the crate-root stage list; see the
/// [module table](crate::streaming) for the emission order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StageKind {
    /// Stage 1 — parse + verify the request's capability token.
    CapTokenVerify,
    /// Stage 2 — evaluate the turn against the Cedar policy bundle.
    CedarCheck,
    /// Stage 4.5 — scan the outbound prompt (and each tool's output) through the
    /// injection-defense filter registry.
    InjectionScan,
    /// Stage 3 — admit the projected envelope against the holder's budget.
    CostGateAdmit,
    /// Stage 5 — the provider's incremental token stream.
    ProviderStream,
    /// Stage 6 — invoke the tools the model requested this round.
    ToolExec,
    /// Stage 7 — mint + sign the turn's receipt and chain it.
    ReceiptMint,
    /// Stage 8 — settle the cost reservation against the actual spend.
    CostGateFinalize,
    /// Stage 9 — record the turn as a bi-temporal memory fact.
    MemoryRecord,
    /// Stage 10 — append the turn to the durable session journal.
    JournalAppend,
}

/// One progressive event from [`FusedRuntime::stream`](crate::FusedRuntime::stream).
///
/// See the [module docs](crate::streaming) for the event model and the
/// stage-emission order. Errors are **not** a variant here — a stage that
/// rejects the turn surfaces as the stream's terminal `Err(RuntimeError)` item,
/// matching the [`ProviderStream`](ardur_provider_runtime::ProviderStream)
/// convention.
#[derive(Clone, Debug, PartialEq)]
pub enum FusedEvent {
    /// A pipeline stage is about to run.
    StageStart {
        /// The stage entering.
        stage: StageKind,
    },
    /// A pipeline stage finished. `ok` is `false` only on the stage that is
    /// about to abort the turn — the terminal `Err` follows immediately.
    StageEnd {
        /// The stage that finished.
        stage: StageKind,
        /// Whether the stage passed (`true`) or is aborting the turn (`false`).
        ok: bool,
    },
    /// A chunk of generated assistant text, forwarded from the provider's
    /// [`StreamEvent::ContentDelta`](ardur_provider_runtime::StreamEvent::ContentDelta)
    /// as it arrives.
    Content(String),
    /// The model began a tool call: its id and name are known; the arguments
    /// follow as [`ToolCallDelta`](FusedEvent::ToolCallDelta) fragments.
    ToolCallStart {
        /// Provider-assigned id of the call.
        id: String,
        /// The tool's name (a registry tool id).
        name: String,
    },
    /// A fragment of a tool call's JSON arguments, keyed to the call `id`.
    ToolCallDelta {
        /// The id of the tool call this fragment belongs to.
        id: String,
        /// A partial-JSON fragment to append to that call's argument buffer.
        delta: String,
    },
    /// A tool finished executing: its structured JSON output, keyed to the call
    /// `id`. Emitted after the runtime invoked the tool (stage 6), scanned its
    /// output, and before the next provider round folds it back in.
    ToolCallResult {
        /// The id of the tool call this result answers.
        id: String,
        /// The tool's structured JSON output.
        result: serde_json::Value,
    },
    /// The token ledger for the round just completed (priced into the receipt).
    Usage(Usage),
    /// A receipt was minted (stage 7) for the round just completed. `chain_hash`
    /// is the lowercase-hex SHA-256 of the receipt's compact JWS — the value the
    /// next receipt's `parent_hash` chains onto, so a verifier can confirm the
    /// linkage off this event alone.
    Receipt {
        /// The minted receipt's id.
        receipt_id: ReceiptId,
        /// Lowercase-hex SHA-256 of the receipt's compact JWS (the chain tail).
        chain_hash: String,
    },
    /// Generation finished; carries the terminal [`FinishReason`] of the final
    /// provider round. The last `Ok` event a clean turn emits.
    Finish(FinishReason),
}
