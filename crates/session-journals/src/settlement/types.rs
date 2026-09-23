//! Serializable evidence, not a capability to debit/refund a budget.

use crate::{CostTuple, ReceiptId, SessionId, UnixTsMillis};
use ardur_core_types::{HolderId, Sha256Digest, TokenId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// Current on-disk settlement schema.
pub const SETTLEMENT_SCHEMA_VERSION: u32 = 1;
/// Stable turn identity; retries must reuse it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnId(pub Uuid);
/// A process-local budget lifetime. Reopening evidence never replays debits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BudgetEpoch(pub Uuid);
/// The existing reservation UUID, not a separately minted retry identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SettlementId(pub Uuid);

/// Latest cumulative evidence for one turn, not an additive money event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnObligation {
    /// Schema version.
    pub schema_version: u32,
    /// Stable turn UUID.
    pub turn_id: TurnId,
    /// Budget lifetime.
    pub budget_epoch: BudgetEpoch,
    /// Request attribution.
    pub request_session: SessionId,
    /// Configured journal owner, independently of request attribution.
    pub journal_owner: Option<SessionId>,
    /// Verified requesting subject.
    pub verified_subject: HolderId,
    /// Actual trusted provisioned budget holder.
    pub budget_holder: HolderId,
    /// Token identifier, never a bearer token.
    pub cap_token_id: TokenId,
    /// Turn start.
    pub started_at: UnixTsMillis,
    /// Ordered cumulative rounds, starting at ordinal zero.
    pub rounds: Vec<RoundObligation>,
    /// Turn disposition.
    pub terminal: TurnTerminal,
    /// Cost-neutral cancellation receipt projection.
    pub cancellation_marker: MarkerProjection,
}

/// One reserved round's known expense and distinct economic disposition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoundObligation {
    /// Existing reservation UUID.
    pub settlement_id: SettlementId,
    /// Zero-based position within the turn.
    pub ordinal: u32,
    /// Actual provider request UUID.
    pub provider_request_id: Uuid,
    /// Provider label.
    pub provider: String,
    /// Model label.
    pub model: String,
    /// Digest only, never a request body.
    pub request_digest: Sha256Digest,
    /// Reservation estimate.
    pub reserved: CostTuple,
    /// Latest cumulative provider observation (not a sum of usage updates).
    pub provider_evidence: ProviderEvidence,
    /// Ordered tool evidence; ordinal distinguishes repeated call IDs.
    pub tools: Vec<ToolEvidence>,
    /// Provider cost plus known completed-tool cost on all five axes.
    pub known_incurred: CostTuple,
    /// Frozen outcome selection, if any.
    pub decision: Option<SettlementDecision>,
    /// Settlement progress; prepared evidence is not completion.
    pub phase: SettlementPhase,
    /// Global projection order allocated by the runtime coordinator.
    pub commit_ordinal: Option<u64>,
    /// Secondary journal projection status.
    pub projection: JournalProjection,
}

/// Provider's latest cumulative usage snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageSnapshot {
    /// Reported input tokens.
    pub input_tokens: u64,
    /// Reported output tokens.
    pub output_tokens: u64,
}
/// How a cost observation was obtained; pricing is not an invoice guarantee.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CostProvenance {
    /// Explicit cost in a completion response.
    ResponseCost,
    /// Explicit provider-reported cost.
    ReportedCost,
    /// Usage priced against this exact rate-card digest.
    PricedUsage(Sha256Digest),
}
/// Evidence distinguishes no dispatch from possible execution/unknown tail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ProviderEvidence {
    /// No dispatch took place.
    NotDispatched,
    /// Dispatch was intended; execution and billing are not yet known.
    DispatchIntent,
    /// An observed cumulative cost; a later unreported tail may remain unknown.
    Observed {
        /// Reported usage, when available.
        usage: Option<UsageSnapshot>,
        /// Latest known cost.
        cost: CostTuple,
        /// Source of this cost.
        provenance: CostProvenance,
        /// Provider reported completion.
        finished: bool,
        /// The operation was interrupted, potentially leaving an unknown tail.
        interrupted: bool,
    },
}
/// Per-call evidence without arguments or output text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolEvidence {
    /// Zero-based position, independent of provider call ID reuse.
    pub ordinal: u32,
    /// Provider call ID.
    pub call_id: String,
    /// Registered tool name.
    pub name: String,
    /// Argument digest only.
    pub arguments_digest: Sha256Digest,
    /// Known or uncertain effect.
    pub effect: ToolEffect,
    /// Scanning may block the output of an already completed effect.
    pub output_admission: OutputAdmission,
}
/// A tool error does not imply that no external effect occurred.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ToolEffect {
    /// Rejected before invoke.
    NotInvoked(RefusalClass),
    /// Persisted intent, not proof of execution.
    DispatchIntent,
    /// Successful returned result, even if subsequent scanning rejects it.
    Completed {
        /// Output digest only.
        output_digest: Sha256Digest,
        /// Observed successful tool cost.
        cost: CostTuple,
    },
    /// Tool returned failure without a billable result.
    Failed {
        /// Typed failure class.
        class: ToolFailureClass,
        /// An external effect may have occurred.
        effect_unknown: bool,
    },
    /// Interrupted after possible dispatch; no result was observed.
    InterruptedUnknown,
}
/// Admission of a tool's returned output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputAdmission {
    /// No scanner decision yet.
    NotScanned,
    /// Output admitted.
    Allowed,
    /// Output rejected (does not undo the effect).
    Blocked,
    /// The scanner failed operationally — no allow/block decision exists.
    /// Never readable as a policy block.
    Undetermined,
}
/// Stable refusal classifications, not diagnostic reason strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefusalClass {
    /// Capability or policy denial.
    Authorization,
    /// No registered tool.
    UnknownTool,
    /// Required grant is missing.
    MissingCapability,
    /// Approval required but unavailable.
    ApprovalRequired,
    /// Approval explicitly rejected.
    ApprovalRejected,
    /// The approval machinery failed operationally (store or receipt append
    /// error) — no human decision was made. Never readable as a rejection.
    ApprovalError,
    /// Returned output blocked by scanning.
    OutputBlocked,
    /// The output scanner failed operationally — no policy decision was
    /// made. Distinct from a block: nothing was refused on policy grounds.
    ScannerError,
    /// Iteration limit exhausted.
    IterationLimit,
    /// Evidence capacity exhausted.
    Capacity,
}
/// Tool execution failures without invented successful-call costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolFailureClass {
    /// Invocation returned an execution error.
    Execution,
    /// Invocation exceeded its deadline.
    Timeout,
    /// Invocation was rejected by its adapter.
    Refused,
}
/// Infrastructure failure classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InfrastructureFailureClass {
    /// Provider transport or protocol failed.
    Provider,
    /// Receipt preparation failed.
    Signing,
    /// Authoritative storage failed.
    Storage,
    /// Budget application failed.
    Budget,
}
/// Frozen round decision, separate from progress and tool outcomes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum SettlementDecision {
    /// Successful intermediate or final round.
    Completion {
        /// True only for a final answer, not an intermediate tool round.
        final_answer: bool,
    },
    /// Non-cancellation refusal.
    Refusal(RefusalClass),
    /// Cancellation won precommit arbitration.
    Cancelled,
    /// Infrastructure failure, not caller cancellation.
    InfrastructureFailure(InfrastructureFailureClass),
}
/// Last definite settlement boundary when further progress is uncertain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DefinitePhase {
    /// Reservation exists.
    Reserved,
    /// Known work was observed.
    WorkObserved,
    /// Decision/candidate was prepared, not committed.
    Prepared,
    /// Process-local application facts were recorded.
    Finalized,
}
/// Settlement progress. Deserializing this applies no economic operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum SettlementPhase {
    /// Reserved; no application facts yet.
    Reserved,
    /// Observed work, not yet settled.
    WorkObserved,
    /// Prepared evidence is not a committed completion.
    Prepared {
        /// Exact candidate, if a completion receipt is intended.
        candidate: Option<ReceiptCandidate>,
    },
    /// Process-local mutation occurred; receipt append may still be pending.
    Finalized {
        /// Exact application facts.
        application: DebitApplication,
        /// Prepared receipt, not proof of append.
        candidate: Option<ReceiptCandidate>,
    },
    /// Definite economic outcome; journal acknowledgement may still lag.
    Settled {
        /// Historical application facts, never a replay instruction.
        application: DebitApplication,
        /// Confirmed append binding, authenticated by the receipt layer.
        receipt: Option<ReceiptBinding>,
    },
    /// Retained ambiguity. Application facts must not be discarded.
    Unresolved {
        /// Last definite progress boundary.
        last_definite: DefinitePhase,
        /// Typed uncertainty.
        problem: SettlementProblem,
        /// Known application facts, if available.
        application: Option<DebitApplication>,
        /// Prepared evidence retained for exact resolution.
        candidate: Option<ReceiptCandidate>,
    },
}
/// Actual capacity movement within one process-scoped epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebitApplication {
    /// The budget lifetime in which these facts occurred.
    pub epoch: BudgetEpoch,
    /// Requested caller debit, independently of expense.
    pub requested_debit: CostTuple,
    /// Actual capacity debit before rollback, which may exceed requested debit
    /// or known incurred cost when a reserved credit is headroom-clamped.
    pub applied_debit: CostTuple,
    /// Actual credit of reserved capacity.
    pub reserved_credit: CostTuple,
    /// Actual additional debit beyond the hold.
    pub additional_debit: CostTuple,
    /// Per-axis positive part of requested minus applied debit, never a negative credit.
    pub shortfall: CostTuple,
    /// Actual compensating credit, if applied.
    pub rollback: RollbackStatus,
}
/// Rollback facts; never authority to refund a newly provisioned budget.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RollbackStatus {
    /// No rollback occurred.
    None,
    /// Exact actual credit toward rollback of the FULL applied debit, not an
    /// arbitrary partial refund request. The remaining obligation is
    /// `applied_debit - credited` on every axis. Partial credit is not closure;
    /// retain it in an unresolved settlement phase until all credit is applied.
    Applied(CostTuple),
}
/// Exact prepared signed evidence. This is NEVER proof of append/completion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptCandidate {
    /// Allocated once; retries must not re-mint it.
    pub receipt_id: ReceiptId,
    /// Expected authenticated chain parent.
    pub expected_parent: Option<Sha256Digest>,
    /// Expected byte offset before append.
    pub expected_log_end: u64,
    /// Exact compact signed receipt, not a bearer credential.
    pub jws_compact: String,
    /// Digest of those exact signed bytes.
    pub jws_digest: Sha256Digest,
}
/// Receipt append binding supplied by the authenticating receipt layer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptBinding {
    /// Committed receipt UUID.
    pub receipt_id: ReceiptId,
    /// Committed signed byte digest.
    pub jws_digest: Sha256Digest,
}
/// Keyed secondary journal projection. Its failure never undoes economics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalProjection {
    /// No settlement projection intent has been allocated yet.
    NotRequired,
    /// No journal configured.
    NotConfigured,
    /// Stable projection UUID; target is the turn's journal owner.
    Pending(Uuid),
    /// Same projection UUID acknowledged by the journal.
    Acknowledged(Uuid),
}
/// Cost-neutral cancellation marker projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarkerProjection {
    /// No marker is required.
    NotRequired,
    /// Stable marker receipt UUID.
    Pending(ReceiptId),
    /// Signed candidate, not completion evidence.
    Prepared(ReceiptCandidate),
    /// Actual committed marker.
    Acknowledged(ReceiptBinding),
}
/// Terminal turn classification, independent of consumer delivery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnTerminal {
    /// Still owned by a live turn.
    Open,
    /// Final answer has a committed receipt binding.
    FinalAnswer(ReceiptId),
    /// Non-cancellation refusal.
    Refused(RefusalClass),
    /// Cancellation won before current-round commit.
    Cancelled,
    /// Infrastructure failure.
    Failed(InfrastructureFailureClass),
    /// Terminal disposition remains ambiguous.
    Unresolved(SettlementProblem),
}
/// Receipt log and public signer identity bound to the root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptIdentity {
    /// Absolute actual configured receipt log path; never silently normalized.
    pub receipt_log: PathBuf,
    /// Public signer identity digest (not a key or bearer token).
    pub signer: Sha256Digest,
}
/// Per-record and inventory allocation limits; no history is automatically pruned.
#[derive(Clone, Copy, Debug)]
pub struct SettlementLimits {
    /// Maximum encoded snapshot bytes, checked before decode allocation.
    pub max_record_bytes: usize,
    /// Maximum bytes per metadata label/identity (not the receipt JWS).
    pub max_metadata_bytes: usize,
    /// Maximum rounds per turn.
    pub max_rounds: usize,
    /// Maximum tool entries per round.
    pub max_tools_per_round: usize,
    /// Maximum prepared JWS bytes.
    pub max_receipt_bytes: usize,
    /// Maximum retained snapshots loaded in one inventory; overflow is an error.
    pub max_inventory_records: usize,
}
impl Default for SettlementLimits {
    fn default() -> Self {
        Self {
            max_record_bytes: 1_048_576,
            max_metadata_bytes: 1024,
            max_rounds: 64,
            max_tools_per_round: 128,
            max_receipt_bytes: 65_536,
            max_inventory_records: 100_000,
        }
    }
}
/// Typed storage/accounting uncertainty, never inferred from diagnostic strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SettlementProblem {
    /// Incompatible revisions or bytes.
    Conflict,
    /// Invalid or incomplete stored evidence.
    Corrupt,
    /// Previously acknowledged snapshot disappeared.
    MissingAcknowledged,
    /// Root, lease, receipt log or signer identity changed.
    IdentityChanged,
    /// Resource bound exceeded.
    Bounds,
    /// Unsupported schema version.
    InvalidSchema,
    /// Transition would erase or contradict evidence.
    InvalidTransition,
    /// Storage failed without sufficient absence/durability proof.
    Io,
    /// Re-establishing durability failed.
    SyncFailed,
    /// Exact target verification failed.
    VerificationFailed,
    /// Prior error still quarantines this writer.
    Unhealthy,
    /// Prior process budget application cannot be reconstructed.
    PriorEpochApplicationUnknown,
    /// Reserved credit remains unapplied because budget headroom is exhausted.
    ReleaseCreditPending,
    /// Compensating rollback credit remains partially unapplied.
    RollbackCreditPending,
}
/// Store failures mean unknown coverage, never zero expense.
#[derive(Debug, thiserror::Error)]
pub enum SettlementStoreError {
    /// An independent writer owns the stable lease.
    #[error("settlement writer already active")]
    WriterBusy,
    /// No supported durability implementation on this platform.
    #[error("unsupported settlement durability platform")]
    UnsupportedPlatform,
    /// Invalid schema, identity, bounds, or evidence.
    #[error("invalid settlement evidence: {0:?}")]
    Invalid(SettlementProblem),
    /// Filesystem failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// No-follow atomic primitive failed.
    #[error(transparent)]
    Durability(#[from] ardur_durability::DurabilityError),
    /// Malformed serialized evidence.
    #[error(transparent)]
    Encoding(#[from] serde_json::Error),
}
#[cfg(unix)]
impl From<rustix::io::Errno> for SettlementStoreError {
    fn from(error: rustix::io::Errno) -> Self {
        Self::Io(error.into())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StoredSnapshot {
    pub revision: u64,
    pub payload_digest: Sha256Digest,
    pub payload: TurnObligation,
}
/// Validated immutable encoded bytes and their typed payload. Encode once per attempt.
#[derive(Clone, Debug)]
pub struct EncodedSnapshot {
    pub(super) stored: StoredSnapshot,
    pub(super) bytes: Vec<u8>,
}
impl EncodedSnapshot {
    /// Prepare a versioned snapshot. The returned immutable bytes are the write identity.
    pub fn new(
        revision: u64,
        turn: TurnObligation,
        limits: &SettlementLimits,
    ) -> Result<Self, SettlementStoreError> {
        validate(revision, &turn, limits)?;
        let payload_digest = Sha256Digest::of(&encode_bounded(&turn, limits.max_record_bytes)?);
        let stored = StoredSnapshot {
            revision,
            payload_digest,
            payload: turn,
        };
        let bytes = encode_bounded(&stored, limits.max_record_bytes)?;
        Ok(Self { stored, bytes })
    }
    /// Exact persisted bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Digest of the entire immutable encoding (including revision).
    pub fn digest(&self) -> Sha256Digest {
        Sha256Digest::of(&self.bytes)
    }
    /// Monotonically increasing revision; initial revision is one.
    pub fn revision(&self) -> u64 {
        self.stored.revision
    }
    /// Cumulative typed snapshot. Mutation requires a new encoding/revision.
    pub fn turn(&self) -> &TurnObligation {
        &self.stored.payload
    }
    pub(super) fn decode(
        bytes: Vec<u8>,
        limits: &SettlementLimits,
    ) -> Result<Self, SettlementStoreError> {
        if bytes.len() > limits.max_record_bytes {
            return Err(invalid(SettlementProblem::Bounds));
        }
        let stored: StoredSnapshot = serde_json::from_slice(&bytes)?;
        let canonical = Self::new(stored.revision, stored.payload, limits)?;
        // Includes digest verification, denies unknown nested fields and legacy
        // coercions, and gives same revision/same bytes a single wire meaning.
        if canonical.bytes != bytes {
            return Err(invalid(SettlementProblem::Corrupt));
        }
        Ok(canonical)
    }
}

pub(super) fn invalid(problem: SettlementProblem) -> SettlementStoreError {
    SettlementStoreError::Invalid(problem)
}

pub(super) fn metadata(text: &str, limits: &SettlementLimits) -> Result<(), SettlementStoreError> {
    if text.len() > limits.max_metadata_bytes {
        return Err(invalid(SettlementProblem::Bounds));
    }
    if text.is_empty() || text.chars().any(char::is_control) {
        return Err(invalid(SettlementProblem::Corrupt));
    }
    Ok(())
}

fn candidate(
    value: &ReceiptCandidate,
    limits: &SettlementLimits,
) -> Result<(), SettlementStoreError> {
    if value.jws_compact.len() > limits.max_receipt_bytes {
        return Err(invalid(SettlementProblem::Bounds));
    }
    if value.receipt_id.0.is_nil()
        || Sha256Digest::of(value.jws_compact.as_bytes()) != value.jws_digest
        || value.jws_compact.split('.').count() != 3
        || value.jws_compact.split('.').any(str::is_empty)
        || !value
            .jws_compact
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(invalid(SettlementProblem::Corrupt));
    }
    Ok(())
}

pub(super) fn application(phase: &SettlementPhase) -> Option<&DebitApplication> {
    match phase {
        SettlementPhase::Finalized { application, .. }
        | SettlementPhase::Settled { application, .. } => Some(application),
        SettlementPhase::Unresolved { application, .. } => application.as_ref(),
        _ => None,
    }
}

pub(super) fn prepared(phase: &SettlementPhase) -> Option<&ReceiptCandidate> {
    match phase {
        SettlementPhase::Prepared { candidate }
        | SettlementPhase::Finalized { candidate, .. }
        | SettlementPhase::Unresolved { candidate, .. } => candidate.as_ref(),
        _ => None,
    }
}

fn positive_difference(a: CostTuple, b: CostTuple) -> CostTuple {
    CostTuple {
        tokens_in: a.tokens_in.saturating_sub(b.tokens_in),
        tokens_out: a.tokens_out.saturating_sub(b.tokens_out),
        cents: a.cents.saturating_sub(b.cents),
        wall_ms: a.wall_ms.saturating_sub(b.wall_ms),
        attention_score: a.attention_score.saturating_sub(b.attention_score),
    }
}

pub(super) fn validate(
    revision: u64,
    turn: &TurnObligation,
    limits: &SettlementLimits,
) -> Result<(), SettlementStoreError> {
    if turn.schema_version != SETTLEMENT_SCHEMA_VERSION {
        return Err(invalid(SettlementProblem::InvalidSchema));
    }
    if revision == 0
        || turn.turn_id.0.is_nil()
        || turn.budget_epoch.0.is_nil()
        || turn.cap_token_id.0.is_nil()
    {
        return Err(invalid(SettlementProblem::Corrupt));
    }
    metadata(&turn.verified_subject.0, limits)?;
    metadata(&turn.budget_holder.0, limits)?;
    if turn.rounds.len() > limits.max_rounds {
        return Err(invalid(SettlementProblem::Bounds));
    }
    let mut settlements = std::collections::HashSet::new();
    let mut requests = std::collections::HashSet::new();
    for (index, round) in turn.rounds.iter().enumerate() {
        if round.ordinal as usize != index
            || round.settlement_id.0.is_nil()
            || round.provider_request_id.is_nil()
            || !settlements.insert(round.settlement_id)
            || !requests.insert(round.provider_request_id)
        {
            return Err(invalid(SettlementProblem::Corrupt));
        }
        metadata(&round.provider, limits)?;
        metadata(&round.model, limits)?;
        if round.tools.len() > limits.max_tools_per_round {
            return Err(invalid(SettlementProblem::Bounds));
        }
        let mut known = match &round.provider_evidence {
            ProviderEvidence::Observed { cost, .. } => *cost,
            _ => CostTuple::ZERO,
        };
        for (index, tool) in round.tools.iter().enumerate() {
            metadata(&tool.call_id, limits)?;
            metadata(&tool.name, limits)?;
            if tool.ordinal as usize != index {
                return Err(invalid(SettlementProblem::Corrupt));
            }
            if let ToolEffect::Completed { cost, .. } = tool.effect {
                known = known
                    .checked_add(&cost)
                    .ok_or(invalid(SettlementProblem::Corrupt))?;
            } else if tool.output_admission != OutputAdmission::NotScanned {
                return Err(invalid(SettlementProblem::Corrupt));
            }
        }
        if known != round.known_incurred {
            return Err(invalid(SettlementProblem::Corrupt));
        }
        if let Some(app) = application(&round.phase) {
            if app.epoch != turn.budget_epoch
                || positive_difference(app.requested_debit, app.applied_debit) != app.shortfall
                || round
                    .reserved
                    .checked_sub(&app.reserved_credit)
                    .and_then(|held| held.checked_add(&app.additional_debit))
                    != Some(app.applied_debit)
                || !known.covers(&app.requested_debit)
            {
                return Err(invalid(SettlementProblem::Corrupt));
            }
            if let RollbackStatus::Applied(credited) = app.rollback {
                if !app.applied_debit.covers(&credited) {
                    return Err(invalid(SettlementProblem::Corrupt));
                }
            }
        }
        if let Some(value) = prepared(&round.phase) {
            candidate(value, limits)?;
        }
        if matches!(
            round.phase,
            SettlementPhase::Prepared { .. }
                | SettlementPhase::Finalized { .. }
                | SettlementPhase::Settled { .. }
        ) && round.decision.is_none()
        {
            return Err(invalid(SettlementProblem::Corrupt));
        }
        if let SettlementPhase::Settled {
            application,
            receipt,
        } = &round.phase
        {
            if incomplete_rollback(application) || incomplete_release(round.reserved, application) {
                return Err(invalid(SettlementProblem::Corrupt));
            }
            if round.decision == Some(SettlementDecision::Cancelled)
                && (application.requested_debit != CostTuple::ZERO
                    || (application.applied_debit != CostTuple::ZERO
                        && !fully_rolled_back(&round.phase)))
            {
                return Err(invalid(SettlementProblem::Corrupt));
            }
            if matches!(round.decision, Some(SettlementDecision::Completion { .. }))
                != receipt.is_some()
                && !fully_rolled_back(&round.phase)
            {
                return Err(invalid(SettlementProblem::Corrupt));
            }
        }
        if let JournalProjection::Pending(id) | JournalProjection::Acknowledged(id) =
            round.projection
        {
            if turn.journal_owner.is_none() || id.is_nil() || round.commit_ordinal.is_none() {
                return Err(invalid(SettlementProblem::Corrupt));
            }
        }
    }
    if let MarkerProjection::Prepared(value) = &turn.cancellation_marker {
        candidate(value, limits)?;
    }
    if let TurnTerminal::FinalAnswer(id) = turn.terminal {
        if !turn.rounds.last().is_some_and(|r| matches!(&r.phase, SettlementPhase::Settled { receipt: Some(binding), .. } if binding.receipt_id == id)
            && r.decision == Some(SettlementDecision::Completion { final_answer: true })) {
            return Err(invalid(SettlementProblem::Corrupt));
        }
    }
    Ok(())
}

// Bound allocation as serde emits bytes; do not allocate a huge temporary JSON
// document and only then decide that it exceeded the record limit.
struct BoundedEncoding {
    bytes: Vec<u8>,
    limit: usize,
}
impl std::io::Write for BoundedEncoding {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("settlement encoding bound"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(super) fn encode_bounded(
    value: &impl Serialize,
    limit: usize,
) -> Result<Vec<u8>, SettlementStoreError> {
    let mut sink = BoundedEncoding {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut sink, value).map_err(|e| {
        if e.is_io() {
            invalid(SettlementProblem::Bounds)
        } else {
            e.into()
        }
    })?;
    Ok(sink.bytes)
}

fn incomplete_rollback(application: &DebitApplication) -> bool {
    matches!(application.rollback, RollbackStatus::Applied(credited)
        if credited != application.applied_debit)
}

fn incomplete_release(reserved: CostTuple, application: &DebitApplication) -> bool {
    // Finalization returns only the excess hold; a zero requested debit returns
    // the entire reserve. Under-collection on other axes is not pending credit.
    // A full compensating rollback independently discharges the remaining hold.
    application.rollback != RollbackStatus::Applied(application.applied_debit)
        && !application
            .reserved_credit
            .covers(&positive_difference(reserved, application.requested_debit))
}

fn fully_rolled_back(phase: &SettlementPhase) -> bool {
    matches!(phase, SettlementPhase::Settled { application, receipt: None }
        if application.rollback == RollbackStatus::Applied(application.applied_debit))
}

fn provider_progress(before: &ProviderEvidence, after: &ProviderEvidence) -> bool {
    match (before, after) {
        (ProviderEvidence::NotDispatched, _) => true,
        (
            ProviderEvidence::DispatchIntent,
            ProviderEvidence::DispatchIntent | ProviderEvidence::Observed { .. },
        ) => true,
        (
            ProviderEvidence::Observed {
                usage: old_usage,
                cost: old_cost,
                provenance: old_source,
                finished: old_finished,
                interrupted: old_interrupted,
            },
            ProviderEvidence::Observed {
                usage,
                cost,
                provenance,
                finished,
                interrupted,
            },
        ) => {
            // One cumulative series: changing its source would erase provenance.
            // Completion and interruption are sticky historical facts. A late
            // result may add completion, but must retain the observed interruption.
            let usage_progress = match (old_usage, usage) {
                (None, _) => true,
                (Some(old), Some(new)) => {
                    new.input_tokens >= old.input_tokens && new.output_tokens >= old.output_tokens
                }
                (Some(_), None) => false,
            };
            cost.covers(old_cost)
                && usage_progress
                && provenance == old_source
                && (!old_finished || *finished)
                && (!old_interrupted || *interrupted)
        }
        _ => false,
    }
}

pub(super) fn transition(
    old: &TurnObligation,
    next: &TurnObligation,
) -> Result<(), SettlementStoreError> {
    let fail = || invalid(SettlementProblem::InvalidTransition);
    if old.turn_id != next.turn_id
        || old.budget_epoch != next.budget_epoch
        || old.request_session != next.request_session
        || old.journal_owner != next.journal_owner
        || old.verified_subject != next.verified_subject
        || old.budget_holder != next.budget_holder
        || old.cap_token_id != next.cap_token_id
        || old.started_at != next.started_at
        || old.rounds.len() > next.rounds.len()
    {
        return Err(fail());
    }
    if !matches!(
        old.terminal,
        TurnTerminal::Open | TurnTerminal::Unresolved(_)
    ) && old.terminal != next.terminal
    {
        return Err(fail());
    }
    let marker_progress = match (&old.cancellation_marker, &next.cancellation_marker) {
        (MarkerProjection::NotRequired, MarkerProjection::Pending(_)) => true,
        (MarkerProjection::Pending(id), MarkerProjection::Prepared(candidate)) => {
            *id == candidate.receipt_id
        }
        (MarkerProjection::Prepared(candidate), MarkerProjection::Acknowledged(binding)) => {
            candidate.receipt_id == binding.receipt_id && candidate.jws_digest == binding.jws_digest
        }
        (a, b) => a == b,
    };
    if !marker_progress {
        return Err(fail());
    }
    // Include newly appended rounds, not only rounds with a predecessor.
    // Settled economics cannot retain an unpaid release or rollback on any
    // decision, with or without a supplied receipt binding.
    for round in &next.rounds {
        if matches!(&round.phase, SettlementPhase::Settled { application, .. }
            if incomplete_rollback(application) || incomplete_release(round.reserved, application))
        {
            return Err(fail());
        }
    }
    for (old, next) in old.rounds.iter().zip(&next.rounds) {
        if old.settlement_id != next.settlement_id
            || old.ordinal != next.ordinal
            || old.provider_request_id != next.provider_request_id
            || old.provider != next.provider
            || old.model != next.model
            || old.request_digest != next.request_digest
            || old.reserved != next.reserved
        {
            return Err(fail());
        }
        if old.decision.is_some() && old.decision != next.decision {
            return Err(fail());
        }
        if old.commit_ordinal.is_some() && old.commit_ordinal != next.commit_ordinal {
            return Err(fail());
        }
        if !provider_progress(&old.provider_evidence, &next.provider_evidence)
            || !next.known_incurred.covers(&old.known_incurred)
            || old.tools.len() > next.tools.len()
        {
            return Err(fail());
        }
        for (before, after) in old.tools.iter().zip(&next.tools) {
            if before.ordinal != after.ordinal
                || before.call_id != after.call_id
                || before.name != after.name
                || before.arguments_digest != after.arguments_digest
            {
                return Err(fail());
            }
            if matches!(
                before.effect,
                ToolEffect::Completed { .. }
                    | ToolEffect::Failed { .. }
                    | ToolEffect::NotInvoked(_)
            ) && before.effect != after.effect
            {
                return Err(fail());
            }
            if before.effect == ToolEffect::InterruptedUnknown
                && !matches!(
                    after.effect,
                    ToolEffect::InterruptedUnknown
                        | ToolEffect::Completed { .. }
                        | ToolEffect::Failed {
                            effect_unknown: true,
                            ..
                        }
                )
            {
                return Err(fail());
            }
            if before.output_admission != OutputAdmission::NotScanned
                && before.output_admission != after.output_admission
            {
                return Err(fail());
            }
        }
        if let Some(before) = application(&old.phase) {
            let after = application(&next.phase).ok_or_else(fail)?;
            let mut expected = before.clone();
            // Only an explicitly pending reserved credit may move these actual
            // components forward. A settled round remains immutable below.
            if matches!(
                old.phase,
                SettlementPhase::Unresolved {
                    problem: SettlementProblem::ReleaseCreditPending,
                    ..
                }
            ) && after.reserved_credit.covers(&before.reserved_credit)
                && before.applied_debit.covers(&after.applied_debit)
            {
                expected.reserved_credit = after.reserved_credit;
                expected.applied_debit = after.applied_debit;
                expected.shortfall = after.shortfall;
            }
            let advancing_rollback = matches!(
                old.phase,
                SettlementPhase::Unresolved {
                    problem: SettlementProblem::RollbackCreditPending,
                    ..
                }
            ) && matches!((&before.rollback, &after.rollback), (RollbackStatus::Applied(a), RollbackStatus::Applied(b)) if b.covers(a));
            if before.rollback == RollbackStatus::None || advancing_rollback {
                expected.rollback = after.rollback.clone();
            }
            if &expected != after {
                return Err(fail());
            }
        }
        if let Some(before) = prepared(&old.phase) {
            let retained = prepared(&next.phase) == Some(before);
            let bound = matches!(&next.phase, SettlementPhase::Settled { receipt: Some(binding), .. }
                if binding.receipt_id == before.receipt_id && binding.jws_digest == before.jws_digest);
            if !retained && !bound && !fully_rolled_back(&next.phase) {
                return Err(fail());
            }
        }
        let new_intent = matches!(
            (&old.projection, &next.projection),
            (
                JournalProjection::NotRequired,
                JournalProjection::Pending(_)
            )
        ) && !matches!(old.phase, SettlementPhase::Settled { .. });
        if old.projection != next.projection
            && !new_intent
            && !matches!((&old.projection, &next.projection),
            (JournalProjection::Pending(a), JournalProjection::Acknowledged(b)) if a == b)
        {
            return Err(fail());
        }
        if matches!(old.phase, SettlementPhase::Settled { .. }) {
            let mut projection_update = old.clone();
            projection_update.projection = next.projection.clone();
            if &projection_update != next {
                return Err(fail());
            }
        }
    }
    Ok(())
}

/// Synchronous exact-write outcome, not an economic action result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteResolution {
    /// Revision durably acknowledged after file AND parent sync.
    Durable(u64),
    /// Exact attempt proved absent without missing prior acknowledged evidence.
    DefinitelyNotApplied,
    /// Cannot establish an exact outcome; writer admission remains latched closed.
    Unresolved(SettlementProblem),
}
/// Writer admission health; there is intentionally no arbitrary reset API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreHealth {
    /// No unresolved storage failure.
    Healthy,
    /// Evidence is quarantined pending exact resolution or operator action.
    Unhealthy(SettlementProblem),
}
/// A directory scan cannot prove undetectable historical deletion did not occur.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InventoryCoverage {
    /// Valid retained files only. Neither historical nor cross-file atomic completeness.
    RetainedFilesOnly,
}
/// Validated retained inventory. Never sum multiple revisions as additive money.
#[derive(Debug)]
pub struct SettlementInventory {
    /// Latest cumulative snapshots, sorted by turn UUID.
    pub snapshots: Vec<EncodedSnapshot>,
    /// Explicit bounded coverage claim, even for an empty valid root.
    pub coverage: InventoryCoverage,
}
