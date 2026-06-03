//! ARD-17 — orphan-receipt reconciliation between the receipt log and the
//! session journal.
//!
//! # The durability gap (see scenario §2.E9)
//!
//! The ten-stage [`submit`](crate::FusedRuntime) pipeline persists each turn's
//! signed receipt at **stage 6** (fsynced) and appends the matching journal
//! entries at **stage 10** (documented non-fatal). A crash — or a transient
//! journal I/O error — in the window *after* stage 6 and *before* stage 10
//! commits leaves the receipt durably in the chain with **no journal
//! `AssistantMessage` referencing its `receipt_id`**: an **orphan receipt**. The
//! receipt chain stays hash-linkable (a later turn chains onto the orphan's JWS
//! with no `parent_hash` divergence), but the journal permanently under-counts
//! turns relative to the receipt log.
//!
//! # Why reconciliation lives here, not on `ReceiptChain`
//!
//! The ARD-17 brief sketched `ReceiptChain::reconcile_with_journal` in the
//! `ardur-receipt` crate. That cannot work without inverting the dependency
//! graph: `ardur-receipt` and `ardur-session-journals` are *independent peer
//! substrate crates* (neither depends on the other — the journal references a
//! receipt only by the `ardur_runtime::ReceiptId` newtype, never by an
//! `ardur_receipt` type). Teaching `ReceiptChain` to read a journal would force
//! `ardur-receipt` to depend on `ardur-session-journals`. The fused runtime
//! already sits at the top of the graph depending on *both*, so reconciliation
//! belongs here — alongside [`load_persisted_chain`](crate::load_persisted_chain),
//! which is itself a fused-runtime concern (the receipt crate's `verify_chain`
//! takes `&[SignedReceipt]`, which cannot be reconstructed off disk).
//!
//! # Option C: startup reconciliation sweep
//!
//! Of the three approaches in the brief — (A) journal-before-receipt reordering,
//! (B) atomic two-phase commit, (C) a startup reconciliation sweep — this is
//! **C**, the minimal v1 that closes the visible production hole without
//! changing the pipeline's stage ordering or the receipt-chain semantics. The
//! sweep walks both stores at boot, finds receipts the journal cannot account
//! for, and applies the configured [`ReconciliationStrategy`].
//!
//! The receipt is the source of truth, so the *default* recovery
//! ([`ReconciliationStrategy::AppendSyntheticJournal`]) **heals the journal**
//! (appends a visible recovery entry naming the orphan's `receipt_id`) rather
//! than destroying the durable receipt. [`TruncateOrphans`] is offered for
//! callers that would rather drop the un-journaled tail than carry a recovery
//! marker; it can only remove the orphan *suffix* (a journaled receipt that
//! chains onto an orphan pins it in place, and removing a load-bearing receipt
//! would break the hash chain — that case is a hard
//! [`ReconciliationError::Undecidable`]).
//!
//! [`TruncateOrphans`]: ReconciliationStrategy::TruncateOrphans

use crate::receipts::ReceiptChainError;
use ardur_session_journals::JournalError;

/// How a reconciliation pass heals the orphan receipts it finds.
///
/// The receipt log is the source of truth; the journal is the derived,
/// human-facing view. The default therefore *heals the journal* rather than
/// mutating the receipt chain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReconciliationStrategy {
    /// Detect and report orphans, but change neither store. The boot sequence
    /// still surfaces the count for alerting; recovery is deferred.
    IgnoreOrphans,

    /// **Default.** For each orphan receipt, append a recovery
    /// [`AssistantMessage`](ardur_session_journals::JournalEntry::AssistantMessage)
    /// to the journal that references the orphan's `receipt_id`. A *visible*
    /// recovery that closes the accounting gap and keeps the durable receipt —
    /// the source of truth — untouched. The original assistant content is
    /// unrecoverable (it was never journaled), so the recovery entry carries a
    /// marker, not the lost text.
    #[default]
    AppendSyntheticJournal,

    /// Remove the trailing run of orphan receipts from the receipt log and reset
    /// the in-memory chain tail. Only the orphan *suffix* is removable: a
    /// journaled receipt that chains onto an orphan pins it, so a non-suffix
    /// orphan yields [`ReconciliationError::Undecidable`] rather than a broken
    /// chain.
    TruncateOrphans,
}

/// What a reconciliation pass did (or, under `dry_run`, would have done).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconciliationAction {
    /// No orphans were found — nothing to do.
    NoOrphans,
    /// Orphans were found but left in place: either `dry_run` was set or the
    /// strategy was [`ReconciliationStrategy::IgnoreOrphans`].
    ReportedOnly,
    /// `count` recovery journal entries were appended (one per orphan).
    AppendedSyntheticJournal {
        /// The number of recovery entries appended.
        count: usize,
    },
    /// `count` trailing orphan receipts were truncated from the receipt log.
    TruncatedReceipts {
        /// The number of receipts removed from the log tail.
        count: usize,
    },
}

/// The outcome of one reconciliation sweep over the receipt log + journal.
#[derive(Clone, Debug)]
pub struct ReconciliationReport {
    /// Receipts present in the on-disk chain at the time of the sweep.
    pub receipt_count: usize,
    /// Distinct receipt ids the journal accounts for (via `AssistantMessage`
    /// entries) at the time of the sweep.
    pub journaled_receipt_count: usize,
    /// The receipt ids found in the chain but absent from the journal — the
    /// orphans, as *detected* (before any recovery action).
    pub orphan_receipt_ids: Vec<uuid::Uuid>,
    /// What the pass did about them.
    pub action: ReconciliationAction,
    /// Whether this was a non-mutating dry run.
    pub dry_run: bool,
}

impl ReconciliationReport {
    /// The number of orphan receipts detected.
    #[must_use]
    pub fn orphan_receipt_count(&self) -> usize {
        self.orphan_receipt_ids.len()
    }
}

/// A failure during a reconciliation sweep.
#[derive(Debug, thiserror::Error)]
pub enum ReconciliationError {
    /// The receipt log could not be loaded or decoded.
    #[error("receipt log: {0}")]
    ReceiptChain(#[from] ReceiptChainError),

    /// The journal could not be replayed or appended to.
    #[error("journal: {0}")]
    Journal(#[from] JournalError),

    /// Rewriting the truncated receipt log failed.
    #[error("rewriting receipt log: {0}")]
    Io(std::io::Error),

    /// Reconciliation could not decide a safe recovery — e.g.
    /// [`TruncateOrphans`](ReconciliationStrategy::TruncateOrphans) was asked to
    /// drop an orphan that a later journaled receipt chains onto, which would
    /// break the hash chain.
    #[error("reconciliation undecidable: {reason}")]
    Undecidable {
        /// Why no safe recovery could be applied.
        reason: String,
    },
}
