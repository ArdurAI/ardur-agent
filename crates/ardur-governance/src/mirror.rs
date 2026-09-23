//! The governance-emitter seam the fused runtime calls at its commit decision.
//!
//! #502 Seam B7, Phase 1: an opt-in [`GovernanceEmitter`] receives the
//! already-verified admission facts of a **committed** round and mirrors them
//! as an Ardur-verifiable Execution Receipt (ER). The trait is deliberately
//! tiny and synchronous: the only fact it is ever handed is a round whose
//! native receipt already durably committed, so an implementation never sits
//! on the admission path and can never widen it. Abandoned / cancelled turns
//! never reach this seam — every cancellation path returns before the commit
//! decision, and the terminal cancellation marker the native chain appends for
//! a mid-loop cancel is deliberately NOT mirrored (Phase 1 semantics).
//!
//! The facts bundle carries only what the runtime already established through
//! its existing gates (stage-1 cap-token verification, Cedar authorization,
//! cost admission, tool authorization) plus the digests the native receipt
//! already recorded. There is deliberately no way to mint an ER from
//! unverified or reconstructed inputs through this seam — that durable
//! per-event evidence contract is #543 and stays out of Phase 1.

use ardur_cap_token::VerifiedClaims;
use ardur_core_types::CostTuple;

use crate::evidence::{PostEffectRecord, PreEffectRecord};

/// One tool call recorded on the committed round's native receipt, as the
/// mirror sees it: identifiers plus the **digests the native receipt already
/// computed** (never raw arguments or outputs — those are not carried inline
/// by the native receipt, and the mirror must not invent a second projection
/// of them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirroredToolCall {
    /// The provider-assigned call id.
    pub call_id: String,
    /// The tool that was invoked.
    pub tool_name: String,
    /// Hex SHA-256 of the JSON arguments (native `arguments_digest`).
    pub arguments_digest: String,
    /// Hex SHA-256 of the JSON output (native `output_digest`).
    pub output_digest: String,
    /// The cost the invocation billed, folded into the round's total.
    pub cost: CostTuple,
}

/// The verified admission facts of one **committed** round, handed to a
/// [`GovernanceEmitter`] at the commit decision. Everything here was already
/// established by the runtime's existing gates or recorded on the native
/// receipt — the mirror projects, it never re-derives.
#[derive(Clone, Debug)]
pub struct ErRoundFacts<'a> {
    /// The stage-1 verified cap-token claims (grant id, actor, budget).
    pub claims: &'a VerifiedClaims,
    /// The session the round belongs to (ER `trace_id`).
    pub trace_id: &'a str,
    /// The committed native receipt's id (ER `step_id`).
    pub step_id: &'a str,
    /// The native receipt's issuance time (Unix milliseconds).
    pub timestamp_millis: u64,
    /// The capability the turn was admitted under (e.g. `chat.submit`).
    pub tool: &'a str,
    /// The provider that served the round.
    pub provider: &'a str,
    /// The tool calls the round recorded, digests only.
    pub tool_calls: &'a [MirroredToolCall],
    /// Whether the round durably persisted transcript state (a configured
    /// session journal appended the round's messages). This — not the mere
    /// presence of tool calls — is what a conservative round-level
    /// `side_effect_class` keys off: `InternalWrite` only when durable
    /// session state actually changed.
    pub persisted_transcript: bool,
}

/// Receives committed-round facts and mirrors them as an Execution Receipt.
///
/// Implementations are called **inside the runtime's commit lock**, after the
/// round's native receipt is durable, so a file-backed mirror can chain
/// without forking. They must be side-effect-bounded (a signed append), must
/// not block indefinitely, and — critically — **cannot influence the turn's
/// outcome**: by the time the seam fires the round has already committed, so
/// an error is surfaced to the operator log, never to the caller.
///
/// # Gap-observability obligation
///
/// Returning `Err` means a **committed** round now has no ER. An
/// implementation MUST make that gap observable rather than resume as if
/// nothing happened: minting later ERs would produce a fully verifiable
/// mirror that silently skips the round, which a downstream reader cannot
/// distinguish from continuous compliance. The shipped file-backed emitter's
/// pattern (`ardur_fused_runtime::ErMirrorEmitter`) is to poison the emitter
/// for its lifetime on ANY failure (projection, signing, or append), so every
/// later round also fails loudly until an operator re-opens the log —
/// re-verification at open then reconciles the on-disk state. A durable
/// gap/`insufficient_evidence` marker is the equivalent for implementations
/// that keep writing. A mirror gap reads as absence at the governance plane
/// (`insufficient_evidence`), never as compliance.
pub trait GovernanceEmitter: Send + Sync {
    /// Mirror one committed round.
    ///
    /// # Errors
    ///
    /// Implementation-defined mirror failure (projection, signing, or the
    /// durable append). The runtime logs it and continues — the native
    /// receipt chain remains the source of truth — but the implementation
    /// MUST honor the gap-observability obligation above before returning
    /// `Ok` from any LATER call.
    fn mirror_committed_round(
        &self,
        facts: &ErRoundFacts<'_>,
    ) -> Result<(), crate::GovernanceError>;

    /// #543: durably record the immutable authorization inputs of one
    /// evaluated event **before** its effect runs.
    ///
    /// The runtime calls this at the tool-call boundary, after the last
    /// admission gate and before `invoke` (or at a gate's denial point, where
    /// no effect will ever run). The record lets a post-crash replay
    /// reconstruct the event without re-executing anything; a live ER is
    /// projected later, at the event's terminal point, from the same record.
    ///
    /// This is **not** an admission hook: the runtime ignores the return
    /// value except to log a failure, and never lets it change the turn's
    /// outcome. An implementation that cannot make the record durable must
    /// treat that as a mirror failure under the gap-observability obligation
    /// above (a missing pre-effect record silently degrades crash recovery
    /// to `insufficient_evidence` absence otherwise).
    ///
    /// # Errors
    ///
    /// Implementation-defined durability failure.
    fn record_pre_effect(&self, record: &PreEffectRecord) -> Result<(), crate::GovernanceError>;

    /// #543: durably record the terminal observation of one evaluated event.
    ///
    /// Written at the event's terminal point (observed effect, typed denial,
    /// or an explicitly unknown outcome) and always BEFORE the matching
    /// [`mirror_evaluated_event`](Self::mirror_evaluated_event) call, so the
    /// ER is only ever projected from durable evidence.
    ///
    /// # Errors
    ///
    /// Implementation-defined durability failure.
    fn record_post_effect(&self, record: &PostEffectRecord) -> Result<(), crate::GovernanceError>;

    /// #543: mirror one evaluated event — one ER per evaluated event — from
    /// its durable pre/post-effect records.
    ///
    /// Called at the event's terminal point, including for events whose round
    /// will never commit (a denial after an earlier successful tool, a
    /// timeout, a scan rejection): the verifier contract wants one ER per
    /// evaluated event in both enforce and attest modes, which the
    /// round-level mirror alone cannot supply. The cancellation marker is
    /// still deliberately not an event (Phase 1 semantics), and a tool call
    /// recorded on a committed round's native receipt but never *evaluated*
    /// (the iteration-limit audit entries) mints no event ER either — it was
    /// never authorized or dispatched.
    ///
    /// Must be idempotent per event: a replayed call for an event whose ER is
    /// already chained is a no-op, not a duplicate (the shipped emitter keys
    /// this on the ER `step_id`, which equals the stable event id).
    ///
    /// # Errors
    ///
    /// Implementation-defined mirror failure, under the same
    /// gap-observability obligation as
    /// [`mirror_committed_round`](Self::mirror_committed_round).
    fn mirror_evaluated_event(
        &self,
        pre: &PreEffectRecord,
        post: &PostEffectRecord,
    ) -> Result<(), crate::GovernanceError>;
}
