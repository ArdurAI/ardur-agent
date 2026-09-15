//! The shared on-disk approval-card store.
//!
//! PR #279 (`feat/approvals-endpoints-2026-07-12`) mounted the **decide**
//! half of the approval-gate loop — `GET /approvals`, `POST
//! /approvals/{id}/approve`, `POST /approvals/{id}/reject` — over a shared
//! filesystem store at `<data_dir>/approvals/<id>.json`, the same directory
//! the CLI's `ardur approvals` subcommand already read/wrote as loose
//! `serde_json::Value`. Nothing produced a pending card.
//!
//! This crate is the **propose** half's shared substrate: a typed
//! [`ApprovalCard`]/[`ApprovalStatus`] and an [`ApprovalStore`] that knows how
//! to create, find, list, decide, and claim cards against that same
//! directory. Every mutating transition — [`ApprovalStore::decide`],
//! [`ApprovalStore::claim_execution`], and the post-decision annotations —
//! runs through ONE locked transaction path used by the HTTP decide routes,
//! the `ardur approvals` CLI, and the fused runtime's approval gate, so the
//! id validation, single-winner semantics, and audit behavior stay identical
//! everywhere a card is touched.
//!
//! # Single-winner transitions (gh#497)
//!
//! A read/check/rename sequence is not a transaction: two independent handles
//! can both read `pending`, both decide, and both report success, with the
//! last rename silently winning; and the old idempotent `consume` returned
//! `Ok` for an already-spent card, so an *observation* was indistinguishable
//! from a fresh execution claim. Every state transition below therefore runs
//! while holding an exclusive advisory lock on `<approvals>/.store.lock`, a
//! stable file that is never renamed, replaced, or unlinked while the store
//! is in use — the same cross-process protocol the file-backed deny list
//! uses. Locking the card file itself would not work: the card inode is
//! *replaced* by each atomic rename, so a lock taken on it would not exclude
//! a handle that opened the path after the rename.
//!
//! - [`ApprovalStore::decide`] gives concurrent approve/reject exactly one
//!   durable winner; the loser gets [`ApprovalStoreError::AlreadyDecided`]
//!   and the durable record never flips twice. A `Consumed` card is decided,
//!   so a stale decision can never resurrect it.
//! - [`ApprovalStore::claim_execution`] is the claim-once execution
//!   transition: the first caller transitions `Approved` → `Consumed` and
//!   receives [`ClaimOutcome::Won`]; every later caller receives
//!   [`ClaimOutcome::AlreadySpent`] — an observation that confers **no**
//!   execution authority. The claim is bound to the recorded action,
//!   arguments digest, and (when the card carries one) session, so a card
//!   can never authorize a call it was not issued for. Cards written before
//!   the propose half existed carry no `tool`/`arguments_digest`/`session_id`
//!   (the decide-half era's `{id, status, summary}` shape); such a legacy
//!   card can be *decided* but never *claimed* — the binding fails closed
//!   with [`ApprovalStoreError::BindingMismatch`], and the operator must
//!   approve a fresh, fully-bound card instead.
//!
//! # Crash semantics — at-most-one, never exactly-once
//!
//! The gate consumes the card **before** invoking the tool, so a failed or
//! crashed invocation can never make the spent approval reusable: dispatch
//! is *at-most-one* per approval. After a crash between claim and
//! invocation, the card is `Consumed` with no recorded outcome — the
//! explicit *ambiguous-effect* state ([`EffectState::Ambiguous`]): the effect
//! may or may not have happened, and the card says so rather than implying
//! either. [`ApprovalStore::record_invocation_outcome`] records the call's
//! resolution (`completed`/`failed`) once it returns; it documents how the
//! *call* resolved, never a guarantee about external effects.
//!
//! # Audit obligations (gh#417)
//!
//! A decision is durable in this store before its journal/receipt audit is
//! minted; the two are not one transaction. When the audit half fails (a late
//! revocation or expiry, an unavailable worker, a persistence error), the
//! decision must not be rolled back — but the unmet obligation must be
//! observable and durable, not a log line.
//! [`ApprovalStore::mark_audit_pending`] records the obligation on the card
//! itself; [`ApprovalStore::record_audit_receipt`] clears it and links the
//! minted receipt once the audit lands.
//!
//! The on-disk schema stays intentionally open (PR #279's own `openapi.rs`
//! documents "additional producer-defined fields may be present") —
//! [`ApprovalCard`] adds `tool`, `capability`, `arguments_digest`,
//! `session_id`, `reason`, the claim/effect/audit fields above, and
//! preserves any other producer-defined field verbatim through every rewrite
//! (see [`ApprovalCard::extra`]). Missing propose-half fields deserialize as
//! empty, so a legacy decide-half card still parses here.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// `<approvals_dir>/<id>.json` ids are restricted to this alphabet and
/// length — identical to PR #279's `valid_approval_id`/`MAX_APPROVAL_ID_LEN`
/// in `crates/server/src/routes.rs`, duplicated here (rather than an
/// inter-crate dependency on `ardur-server`, which would invert the
/// dependency direction the server needs) so every producer and consumer of
/// the store enforces the exact same traversal-safety rule.
const MAX_APPROVAL_ID_LEN: usize = 128;

/// The store-wide coordination file: `<approvals>/.store.lock`.
///
/// Stable for the life of the store — never renamed, replaced, truncated, or
/// unlinked — so an advisory lock taken on it serializes every independent
/// handle and process that opens this directory. (Locking a card path would
/// be useless: each write *replaces* that inode via rename.) The leading dot
/// keeps it out of `*.json` listings; it carries no data.
const STORE_LOCK_FILE: &str = ".store.lock";

/// Whether `id` is safe to join onto the approvals directory: non-empty, at
/// most [`MAX_APPROVAL_ID_LEN`] bytes, and drawn only from
/// `[A-Za-z0-9_-]` — no `.`/`/`/NUL, so it can never traverse out of the
/// directory once joined onto a path.
#[must_use]
pub fn valid_approval_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_APPROVAL_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// An approval card's lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalStatus {
    /// Proposed, awaiting an operator decision.
    Pending,
    /// An operator approved it.
    Approved,
    /// An operator rejected it (the wire/CLI verb is `reject`/`deny`; the
    /// stored status, matching PR #279's own reconciliation, is `denied`).
    Denied,
    /// An operator approved it **and the authorized call has since been
    /// claimed**.
    ///
    /// Approval authorises one invocation, not a standing permission. Without
    /// this state `find_matching` keeps returning the same `Approved` card, so
    /// a single approval would let an identical call repeat without limit —
    /// for `shell.run` or `file.write` that is a materially different grant
    /// from the one the operator actually gave.
    ///
    /// A consumed card no longer matches, so the next identical call proposes
    /// a fresh one. The record is kept rather than deleted so the receipt
    /// chain's `approval_id` stays resolvable for audit — including the
    /// crash-ambiguous case: a consumed card with no recorded invocation
    /// outcome is the explicit "effect unknown" state (see
    /// [`ApprovalStore::effect_state`]).
    Consumed,
}

/// Serde helper: skip a zero `created_at` so a legacy card that never
/// carried the field keeps its original shape through a rewrite.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl ApprovalStatus {
    #[must_use]
    pub fn is_decided(self) -> bool {
        !matches!(self, ApprovalStatus::Pending)
    }

    /// The lowercase wire/persistence spelling (the serde form).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalStatus::Pending => "pending",
            ApprovalStatus::Approved => "approved",
            ApprovalStatus::Denied => "denied",
            ApprovalStatus::Consumed => "consumed",
        }
    }
}

/// How a claimed invocation resolved, recorded on the card after the call
/// returns. This documents the *call's* resolution — never a guarantee about
/// external effects (a failed call may still have had one; a completed call's
/// effects are simply what they are).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InvocationResult {
    /// The tool invocation returned successfully.
    Completed,
    /// The tool invocation returned an error. The approval stays spent
    /// (consume-before-invoke): a failed call does not refund the grant.
    Failed,
}

/// The recorded resolution of a claimed invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvocationOutcome {
    /// How the call resolved.
    pub result: InvocationResult,
    /// Unix seconds the call resolved.
    pub finished_at: u64,
}

/// The observed effect state of a card, via [`ApprovalStore::effect_state`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EffectState {
    /// The card was never claimed (pending/approved/denied): no invocation
    /// was authorized by it.
    NotSpent,
    /// The card is `Consumed` with no recorded invocation outcome: the
    /// invocation's effect is **unknown** (the process crashed or the call
    /// never resolved after the claim). This is the explicit ambiguous-effect
    /// state — the store promises at-most-one dispatch, and says so honestly
    /// when it cannot say which way the dispatch went.
    Ambiguous,
    /// The card is `Consumed` and the invocation's resolution was recorded.
    Recorded(InvocationOutcome),
}

/// One approval card, as persisted at `<approvals_dir>/<id>.json`.
///
/// `id` is not itself a field — it is the filename (minus `.json`), injected
/// by the reader after a lookup, matching PR #279's own convention ("each
/// with its `id` injected").
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalCard {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub status: ApprovalStatus,
    /// Unix seconds this card was proposed. Skipped when zero so a legacy
    /// card that never carried it keeps its original shape.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub created_at: u64,
    /// Unix seconds an operator decided this card, once decided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<u64>,
    /// The reason given for a `Denied` decision (empty string, not absent,
    /// for a reject with no reason — matches PR #279's existing convention
    /// so the wire shape does not change for the decide-half's consumers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deny_reason: Option<String>,
    /// The tool the gated call targeted. Absent on legacy decide-half-era
    /// cards; such cards can be decided but never claimed (see the module
    /// docs on legacy treatment). Skipped when empty so a legacy card keeps
    /// its original shape instead of gaining phantom binding fields.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool: String,
    /// The capability that triggered approval-gating for this call.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub capability: String,
    /// `sha256(arguments)` hex digest of the tool call this card gates —
    /// the matching key a retried call is looked up by, so re-submitting
    /// the *same* call after approval proceeds without minting a second
    /// card, and a *different* call against the same tool proposes its own.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub arguments_digest: String,
    /// The session the gated call was made in, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// A short human-readable summary of why approval was requested (e.g.
    /// "tool `shell.run` requires capability `shell.exec`, which is
    /// approval-gated").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// Unix seconds the single execution claim was won, once claimed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumed_at: Option<u64>,
    /// The trusted caller identity the winning claim was recorded under
    /// (the verified cap-token subject), when supplied by the caller.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumed_by: Option<String>,
    /// How the claimed invocation resolved, once it returned. Absent on a
    /// `Consumed` card is the ambiguous-effect state (crash/timeout), not a
    /// missing success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invocation_outcome: Option<InvocationOutcome>,
    /// The decision's minted receipt id, once the audit half landed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt_id: Option<String>,
    /// `true` while a *durable decision* still lacks its journal/receipt
    /// audit — the explicit pending obligation an operator (or a later
    /// recovery pass) can observe and settle. Never manufactured by
    /// disabling verification.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit_pending: Option<bool>,
    /// Why the audit obligation is pending (the failing stage and error).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit_pending_reason: Option<String>,
    /// Producer-defined fields this crate does not know (PR #279's open
    /// schema — e.g. the decide-half era's `summary`), preserved verbatim
    /// through every rewrite so routing a card through the typed store never
    /// silently drops another producer's data.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A decision an operator can make against a pending card.
#[derive(Clone, Debug)]
pub enum Decision {
    Approve,
    Reject { reason: String },
}

/// The execution claim's binding: the action/arguments/session the caller
/// asserts, checked against the card's recorded fields before authority is
/// granted. A claim that does not match what the operator approved fails
/// closed with [`ApprovalStoreError::BindingMismatch`].
#[derive(Clone, Debug)]
pub struct ClaimBinding<'a> {
    /// The tool being invoked.
    pub tool: &'a str,
    /// The `sha256(arguments)` hex digest of the invocation.
    pub arguments_digest: &'a str,
    /// The session the invocation belongs to. Matches exactly: a card that
    /// recorded a session only authorises that session; a card that recorded
    /// none only matches a caller presenting none.
    pub session_id: Option<&'a str>,
    /// The trusted caller identity to record on the claim (the verified
    /// cap-token subject), for audit attribution.
    pub claimed_by: Option<&'a str>,
}

/// The outcome of [`ApprovalStore::claim_execution`].
///
/// Sealed (`#[non_exhaustive]`): only this crate may construct variants,
/// so no downstream caller can fabricate a [`ClaimOutcome::Won`] and
/// present it as execution authority — the outcome is only meaningful as
/// the return value of [`ApprovalStore::claim_execution`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ClaimOutcome {
    /// This caller transitioned the card `Approved` → `Consumed`: it alone
    /// may perform the single invocation this approval authorises.
    Won(Box<ApprovalCard>),
    /// The card was already `Consumed` — by this caller's earlier retry or by
    /// another handle/process. This is an idempotent *observation*, not
    /// authority: the caller must not perform the invocation.
    AlreadySpent(Box<ApprovalCard>),
}

impl ClaimOutcome {
    /// Whether this outcome grants execution authority (only [`ClaimOutcome::Won`]).
    #[must_use]
    pub fn is_won(&self) -> bool {
        matches!(self, ClaimOutcome::Won(_))
    }

    /// The observed card, whichever way the claim went.
    #[must_use]
    pub fn into_card(self) -> ApprovalCard {
        match self {
            ClaimOutcome::Won(card) | ClaimOutcome::AlreadySpent(card) => *card,
        }
    }
}

/// A failure reading, writing, deciding, or claiming an approval card.
#[derive(Debug, thiserror::Error)]
pub enum ApprovalStoreError {
    #[error("invalid approval id")]
    InvalidId,
    #[error("approval card not found")]
    NotFound,
    #[error("approval card already decided")]
    AlreadyDecided,
    #[error("approval card is not approved (status: {0:?})")]
    NotApproved(ApprovalStatus),
    #[error("approval card is not consumed (status: {0:?})")]
    NotConsumed(ApprovalStatus),
    #[error("approval card is not decided yet")]
    NotDecided,
    #[error("claim binding mismatch on `{0}`: the card does not authorise this call")]
    BindingMismatch(&'static str),
    #[error("approval card is corrupt: {0}")]
    Corrupt(String),
    #[error("i/o error reading the approval card: {0}")]
    Read(std::io::Error),
    #[error("i/o error persisting the approval card: {0}")]
    Write(std::io::Error),
}

/// The shared on-disk approval-card store: `<data_dir>/approvals/*.json`.
///
/// Every write goes through the same atomic temp-file-plus-`fsync`-plus-
/// `rename` sequence PR #279's `write_atomically` established, so a reader
/// (the CLI, the HTTP `GET /approvals` handler, or another `ApprovalStore`
/// instance) never observes a torn record. Every *transition* additionally
/// holds the store-wide exclusive lock (see the module docs), so a decision,
/// claim, or annotation is a read/check/write **transaction** with exactly
/// one durable winner across independent handles and processes.
#[derive(Clone, Debug)]
pub struct ApprovalStore {
    dir: PathBuf,
}

impl ApprovalStore {
    /// Open a store rooted at `dir` (typically `<data_dir>/approvals`).
    /// Does not create the directory — callers that propose or decide a
    /// card create it lazily via [`ensure_dir`](Self::ensure_dir).
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory this store is rooted at.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn ensure_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)
    }

    fn path_for(&self, id: &str) -> Result<PathBuf, ApprovalStoreError> {
        if !valid_approval_id(id) {
            return Err(ApprovalStoreError::InvalidId);
        }
        Ok(self.dir.join(format!("{id}.json")))
    }

    /// Run `f` while holding the store-wide exclusive advisory lock.
    ///
    /// The lock file is opened independently on every call: cloned
    /// `ApprovalStore` handles (or two processes) opening the same path get
    /// separate open file descriptions, so the lock excludes them from each
    /// other (the `FileDenyList` protocol). It is never renamed or unlinked,
    /// so the exclusion domain is stable across the atomic card renames the
    /// transactions perform. Dropping the handle releases the lock, including
    /// on a process crash — there is no stale-lock state to recover from.
    fn with_exclusive_lock<T>(
        &self,
        f: impl FnOnce(&Self) -> Result<T, ApprovalStoreError>,
    ) -> Result<T, ApprovalStoreError> {
        self.ensure_dir().map_err(ApprovalStoreError::Write)?;
        let lock_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join(STORE_LOCK_FILE))
            .map_err(ApprovalStoreError::Write)?;
        lock_file.lock().map_err(ApprovalStoreError::Write)?;
        let result = f(self);
        // Unlock eagerly rather than at drop so an error inside `f` never
        // extends the critical section past this function's return.
        lock_file.unlock().map_err(ApprovalStoreError::Write)?;
        result
    }

    /// Read one card by id, with its `id` injected.
    ///
    /// # Errors
    /// [`ApprovalStoreError::InvalidId`] for a malformed id,
    /// [`ApprovalStoreError::NotFound`] if no card exists,
    /// [`ApprovalStoreError::Corrupt`] if the file is not valid JSON.
    pub fn read(&self, id: &str) -> Result<ApprovalCard, ApprovalStoreError> {
        let path = self.path_for(id)?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ApprovalStoreError::NotFound);
            }
            Err(e) => return Err(ApprovalStoreError::Read(e)),
        };
        let mut card: ApprovalCard = serde_json::from_slice(&bytes)
            .map_err(|e| ApprovalStoreError::Corrupt(e.to_string()))?;
        card.id = Some(id.to_string());
        Ok(card)
    }

    /// Every card in the store, each with its `id` injected. Skips (rather
    /// than fails on) a non-`.json` entry or a corrupt card, matching PR
    /// #279's list handler, which surfaces best-effort — a single bad file
    /// should not blank the whole list.
    pub fn list(&self) -> std::io::Result<Vec<ApprovalCard>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut cards = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Ok(card) = self.read(id) {
                cards.push(card);
            }
        }
        Ok(cards)
    }

    /// Find a pending or already-decided card matching `(tool,
    /// arguments_digest)` exactly, and `session_id` when the candidate has
    /// one recorded. Used by the propose path to make re-checking the same
    /// gated call idempotent: a second identical call while a card is still
    /// pending returns the *same* card rather than minting a duplicate, and
    /// a call after approval finds the approved card rather than proposing
    /// again.
    ///
    /// [`Consumed`](ApprovalStatus::Consumed) cards are skipped. An approval
    /// authorises one invocation, so once the authorized call has run its card
    /// must stop matching — otherwise the next identical call would be waved
    /// through on a grant the operator already spent.
    pub fn find_matching(
        &self,
        tool: &str,
        arguments_digest: &str,
        session_id: Option<&str>,
    ) -> std::io::Result<Option<ApprovalCard>> {
        let cards = self.list()?;
        Ok(cards.into_iter().find(|c| {
            c.tool == tool
                && c.arguments_digest == arguments_digest
                && c.session_id.as_deref() == session_id
                && c.status != ApprovalStatus::Consumed
        }))
    }

    /// Create a new `Pending` card with a fresh id and write it atomically.
    /// Returns the card with its `id` injected.
    ///
    /// # Errors
    /// [`ApprovalStoreError::Write`] if the directory cannot be created or the
    /// card cannot be written.
    pub fn propose(
        &self,
        tool: impl Into<String>,
        capability: impl Into<String>,
        arguments_digest: impl Into<String>,
        session_id: Option<String>,
        reason: impl Into<String>,
        created_at: u64,
    ) -> Result<ApprovalCard, ApprovalStoreError> {
        self.ensure_dir().map_err(ApprovalStoreError::Write)?;
        let id = uuid::Uuid::now_v7().to_string();
        let card = ApprovalCard {
            id: Some(id.clone()),
            status: ApprovalStatus::Pending,
            created_at,
            decided_at: None,
            deny_reason: None,
            tool: tool.into(),
            capability: capability.into(),
            arguments_digest: arguments_digest.into(),
            session_id,
            reason: reason.into(),
            consumed_at: None,
            consumed_by: None,
            invocation_outcome: None,
            receipt_id: None,
            audit_pending: None,
            audit_pending_reason: None,
            extra: serde_json::Map::new(),
        };
        self.write_atomically(&id, &card)?;
        Ok(card)
    }

    /// Return the existing non-consumed card matching `(tool,
    /// arguments_digest, session_id)`, or atomically propose a fresh
    /// `Pending` one when none exists — as ONE locked transaction.
    ///
    /// The find-then-propose pair the runtime's gate used to run unlocked
    /// could mint duplicate pending cards for the same call under overlap;
    /// doing both inside the lock means the second caller observes the first
    /// caller's card. Returns the card and whether this call created it.
    ///
    /// # Errors
    /// As [`propose`](Self::propose).
    #[allow(clippy::too_many_arguments)]
    pub fn propose_if_absent(
        &self,
        tool: impl Into<String>,
        capability: impl Into<String>,
        arguments_digest: impl Into<String>,
        session_id: Option<String>,
        reason: impl Into<String>,
        created_at: u64,
    ) -> Result<(ApprovalCard, bool), ApprovalStoreError> {
        let tool = tool.into();
        let capability = capability.into();
        let arguments_digest = arguments_digest.into();
        let reason = reason.into();
        self.with_exclusive_lock(|store| {
            if let Some(card) = store
                .find_matching(&tool, &arguments_digest, session_id.as_deref())
                .map_err(ApprovalStoreError::Read)?
            {
                return Ok((card, false));
            }
            let card = store.propose(
                tool,
                capability,
                arguments_digest,
                session_id,
                reason,
                created_at,
            )?;
            Ok((card, true))
        })
    }

    /// Apply `decision` to the `Pending` card `id`, stamp `decided_at`, and
    /// write the result atomically — as ONE locked transaction, so two
    /// concurrent decisions over the same card have exactly one durable
    /// winner. Returns the updated card.
    ///
    /// The same locked write also stamps the durable pending-audit obligation
    /// (`audit_pending`): the decision and the record of its unmet journal/
    /// receipt audit land atomically, so a crash between the decision and the
    /// caller's audit attempts can never leave an approved/denied card with
    /// no audit AND no obligation marker. The caller clears the obligation
    /// ([`record_audit_receipt`](Self::record_audit_receipt)) only after
    /// every required audit step succeeds, or updates the reason
    /// ([`mark_audit_pending`](Self::mark_audit_pending)) with what failed.
    ///
    /// # Errors
    /// [`ApprovalStoreError::InvalidId`]/[`NotFound`](ApprovalStoreError::NotFound)/
    /// [`Corrupt`](ApprovalStoreError::Corrupt) as [`read`](Self::read).
    /// [`ApprovalStoreError::AlreadyDecided`] if `id`'s card is not
    /// currently `Pending` — the explicit conflict a losing decider gets.
    /// Repeated decide calls are idempotent-safe (the mutation fires exactly
    /// once), and a `Consumed` card is decided, so a stale decision can never
    /// resurrect a spent approval.
    pub fn decide(
        &self,
        id: &str,
        decision: Decision,
        decided_at: u64,
    ) -> Result<ApprovalCard, ApprovalStoreError> {
        self.with_exclusive_lock(|store| {
            let mut card = store.read(id)?;
            if card.status.is_decided() {
                return Err(ApprovalStoreError::AlreadyDecided);
            }
            match decision {
                Decision::Approve => {
                    card.status = ApprovalStatus::Approved;
                }
                Decision::Reject { reason } => {
                    card.status = ApprovalStatus::Denied;
                    card.deny_reason = Some(reason);
                }
            }
            card.decided_at = Some(decided_at);
            // The audit obligation is part of the SAME durable write as the
            // decision: between this commit and the caller's journal/receipt
            // attempts the card honestly reports its unmet audit, even across
            // a crash.
            card.audit_pending = Some(true);
            card.audit_pending_reason =
                Some("decision committed; journal/receipt audit not yet recorded".to_string());
            store.write_atomically(id, &card)?;
            Ok(card)
        })
    }

    /// Claim the single execution an `Approved` card authorises, bound to
    /// `binding` — as ONE locked transaction shared by every handle and
    /// process.
    ///
    /// - [`ClaimOutcome::Won`]: this caller transitioned the card to
    ///   `Consumed` (stamping `consumed_at`/`consumed_by`); it alone may
    ///   perform the invocation.
    /// - [`ClaimOutcome::AlreadySpent`]: the card was already consumed. An
    ///   idempotent observation, **not** authority — the caller must not
    ///   invoke, however it reached this state (its own retry after a crash,
    ///   or a losing race).
    ///
    /// Consume-before-invoke is preserved: the card is spent before the tool
    /// runs, so a failed tool or a crash can never make the grant reusable
    /// (at-most-one dispatch; see the module docs for the ambiguous-effect
    /// contract).
    ///
    /// # Errors
    /// [`ApprovalStoreError::NotApproved`] for a `Pending`/`Denied` card
    /// (claiming either would invent an authorisation),
    /// [`ApprovalStoreError::BindingMismatch`] if the card's recorded
    /// action/arguments/session do not match `binding` — the explicit legacy
    /// treatment for cards that predate binding fields, which can therefore
    /// never authorize an execution —
    /// plus the [`read`](Self::read) errors.
    pub fn claim_execution(
        &self,
        id: &str,
        binding: &ClaimBinding<'_>,
        claimed_at: u64,
    ) -> Result<ClaimOutcome, ApprovalStoreError> {
        self.with_exclusive_lock(|store| {
            let mut card = store.read(id)?;
            match card.status {
                ApprovalStatus::Consumed => {
                    return Ok(ClaimOutcome::AlreadySpent(Box::new(card)));
                }
                ApprovalStatus::Approved => {}
                other => return Err(ApprovalStoreError::NotApproved(other)),
            }
            if card.tool.is_empty() || binding.tool.is_empty() || card.tool != binding.tool {
                return Err(ApprovalStoreError::BindingMismatch("tool"));
            }
            if card.arguments_digest.is_empty()
                || binding.arguments_digest.is_empty()
                || card.arguments_digest != binding.arguments_digest
            {
                return Err(ApprovalStoreError::BindingMismatch("arguments_digest"));
            }
            if card.session_id.as_deref() != binding.session_id {
                return Err(ApprovalStoreError::BindingMismatch("session_id"));
            }
            card.status = ApprovalStatus::Consumed;
            card.consumed_at = Some(claimed_at);
            card.consumed_by = binding.claimed_by.map(str::to_string);
            store.write_atomically(id, &card)?;
            Ok(ClaimOutcome::Won(Box::new(card)))
        })
    }

    /// Record how a claimed invocation resolved (consumed-before-invoke means
    /// the card is already spent; this is the post-call audit annotation).
    ///
    /// # Errors
    /// [`ApprovalStoreError::NotConsumed`] if the card is not `Consumed` —
    /// recording an outcome for an unclaimed card would fabricate evidence of
    /// an invocation that was never authorized — plus the
    /// [`read`](Self::read) errors.
    pub fn record_invocation_outcome(
        &self,
        id: &str,
        outcome: InvocationOutcome,
    ) -> Result<ApprovalCard, ApprovalStoreError> {
        self.with_exclusive_lock(|store| {
            let mut card = store.read(id)?;
            if card.status != ApprovalStatus::Consumed {
                return Err(ApprovalStoreError::NotConsumed(card.status));
            }
            card.invocation_outcome = Some(outcome);
            store.write_atomically(id, &card)?;
            Ok(card)
        })
    }

    /// The observed effect state of a card (see [`EffectState`]).
    ///
    /// # Errors
    /// As [`read`](Self::read).
    pub fn effect_state(&self, id: &str) -> Result<EffectState, ApprovalStoreError> {
        let card = self.read(id)?;
        Ok(Self::effect_state_of(&card))
    }

    /// [`effect_state`](Self::effect_state) for an already-loaded card.
    #[must_use]
    pub fn effect_state_of(card: &ApprovalCard) -> EffectState {
        if card.status != ApprovalStatus::Consumed {
            return EffectState::NotSpent;
        }
        match &card.invocation_outcome {
            Some(outcome) => EffectState::Recorded(outcome.clone()),
            None => EffectState::Ambiguous,
        }
    }

    /// Record the durable pending-audit obligation on a decided card: the
    /// decision landed but its journal/receipt audit did not (`reason` names
    /// the failing stage). Idempotent — re-marking updates the reason.
    ///
    /// # Errors
    /// [`ApprovalStoreError::NotDecided`] if the card is still `Pending`
    /// (an audit obligation can only exist for a committed decision), plus
    /// the [`read`](Self::read) errors.
    pub fn mark_audit_pending(
        &self,
        id: &str,
        reason: &str,
    ) -> Result<ApprovalCard, ApprovalStoreError> {
        self.with_exclusive_lock(|store| {
            let mut card = store.read(id)?;
            if !card.status.is_decided() {
                return Err(ApprovalStoreError::NotDecided);
            }
            card.audit_pending = Some(true);
            card.audit_pending_reason = Some(reason.to_string());
            store.write_atomically(id, &card)?;
            Ok(card)
        })
    }

    /// Link a decided card to its minted decision receipt and clear any
    /// pending audit obligation. Idempotent for the same `receipt_id`.
    ///
    /// # Errors
    /// [`ApprovalStoreError::NotDecided`] if the card is still `Pending`,
    /// plus the [`read`](Self::read) errors.
    pub fn record_audit_receipt(
        &self,
        id: &str,
        receipt_id: &str,
    ) -> Result<ApprovalCard, ApprovalStoreError> {
        self.with_exclusive_lock(|store| {
            let mut card = store.read(id)?;
            if !card.status.is_decided() {
                return Err(ApprovalStoreError::NotDecided);
            }
            card.receipt_id = Some(receipt_id.to_string());
            card.audit_pending = None;
            card.audit_pending_reason = None;
            store.write_atomically(id, &card)?;
            Ok(card)
        })
    }

    fn write_atomically(&self, id: &str, card: &ApprovalCard) -> Result<(), ApprovalStoreError> {
        self.ensure_dir().map_err(ApprovalStoreError::Write)?;
        let path = self.path_for(id)?;
        let bytes = serde_json::to_vec_pretty(card)
            .map_err(|e| ApprovalStoreError::Corrupt(e.to_string()))?;
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let tmp = dir.join(format!(".approval-{}.tmp", uuid::Uuid::now_v7().simple()));
        {
            let mut file = std::fs::File::create(&tmp).map_err(ApprovalStoreError::Write)?;
            file.write_all(&bytes).map_err(ApprovalStoreError::Write)?;
            file.sync_all().map_err(ApprovalStoreError::Write)?;
        }
        match std::fs::rename(&tmp, &path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(ApprovalStoreError::Write(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, ApprovalStore) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ApprovalStore::new(dir.path().join("approvals"));
        (dir, store)
    }

    #[test]
    fn propose_writes_a_pending_card_with_an_injected_id() {
        let (_dir, store) = store();
        let card = store
            .propose(
                "shell.run",
                "shell.exec",
                "deadbeef",
                Some("sess-1".to_string()),
                "needs approval",
                1000,
            )
            .expect("propose succeeds");
        assert!(card.id.is_some());
        assert_eq!(card.status, ApprovalStatus::Pending);
        assert_eq!(card.tool, "shell.run");

        let read_back = store
            .read(card.id.as_deref().unwrap())
            .expect("read succeeds");
        assert_eq!(read_back.arguments_digest, "deadbeef");
    }

    #[test]
    fn find_matching_locates_the_right_card_and_ignores_others() {
        let (_dir, store) = store();
        let target = store
            .propose(
                "shell.run",
                "shell.exec",
                "aaa",
                Some("sess-1".to_string()),
                "r",
                1,
            )
            .unwrap();
        store
            .propose(
                "shell.run",
                "shell.exec",
                "bbb",
                Some("sess-1".to_string()),
                "r",
                1,
            )
            .unwrap();
        store
            .propose(
                "shell.run",
                "shell.exec",
                "aaa",
                Some("sess-2".to_string()),
                "r",
                1,
            )
            .unwrap();

        let found = store
            .find_matching("shell.run", "aaa", Some("sess-1"))
            .expect("find succeeds")
            .expect("a match exists");
        assert_eq!(found.id, target.id);
    }

    #[test]
    fn decide_approve_flips_status_and_stamps_decided_at() {
        let (_dir, store) = store();
        let card = store.propose("t", "c", "d", None, "r", 1).unwrap();
        let id = card.id.clone().unwrap();

        let decided = store
            .decide(&id, Decision::Approve, 42)
            .expect("decide succeeds");
        assert_eq!(decided.status, ApprovalStatus::Approved);
        assert_eq!(decided.decided_at, Some(42));
    }

    #[test]
    fn decide_reject_records_the_reason() {
        let (_dir, store) = store();
        let card = store.propose("t", "c", "d", None, "r", 1).unwrap();
        let id = card.id.clone().unwrap();

        let decided = store
            .decide(
                &id,
                Decision::Reject {
                    reason: "too risky".to_string(),
                },
                42,
            )
            .expect("decide succeeds");
        assert_eq!(decided.status, ApprovalStatus::Denied);
        assert_eq!(decided.deny_reason.as_deref(), Some("too risky"));
    }

    #[test]
    fn decide_twice_is_rejected_not_silently_overwritten() {
        let (_dir, store) = store();
        let card = store.propose("t", "c", "d", None, "r", 1).unwrap();
        let id = card.id.clone().unwrap();

        store.decide(&id, Decision::Approve, 1).unwrap();
        let second = store.decide(&id, Decision::Approve, 2);
        assert!(matches!(second, Err(ApprovalStoreError::AlreadyDecided)));
    }

    #[test]
    fn read_unknown_id_is_not_found() {
        let (_dir, store) = store();
        let err = store
            .read("00000000-0000-0000-0000-000000000000")
            .unwrap_err();
        assert!(matches!(err, ApprovalStoreError::NotFound));
    }

    #[test]
    fn traversal_shaped_ids_are_rejected() {
        let (_dir, store) = store();
        for bad in ["../etc/passwd", "a/b", "a.json", ""] {
            assert!(
                matches!(store.read(bad), Err(ApprovalStoreError::InvalidId)),
                "id {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn list_is_empty_for_a_missing_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ApprovalStore::new(dir.path().join("does-not-exist"));
        assert!(store.list().expect("list succeeds").is_empty());
    }

    #[test]
    fn list_returns_every_proposed_card() {
        let (_dir, store) = store();
        store.propose("a", "c", "1", None, "r", 1).unwrap();
        store.propose("b", "c", "2", None, "r", 1).unwrap();
        assert_eq!(store.list().unwrap().len(), 2);
    }
}
