//! ARD-17 — orphan-receipt reconciliation unit tests.
//!
//! These exercise [`FusedRuntime::reconcile_receipts`] directly, in isolation
//! from the mid-turn-crash machinery scenario §2.E9 uses. The orphan is
//! manufactured the simplest faithful way: run clean turns through a
//! receipt-log + journal pair, then **truncate the journal's last
//! `AssistantMessage` line off disk** — leaving that turn's receipt durable in
//! the chain with no journal entry referencing it. That is byte-for-byte the
//! state a crash in the stage-6→10 window leaves behind (receipt fsynced at
//! stage 6, journal append at stage 10 never committed), without needing a
//! panicking journal decorator.
//!
//! A fresh runtime is then built over the same paths — the realistic restart —
//! so the journal's entry counter is recomputed from the truncated file rather
//! than carrying a stale in-memory count.

use std::path::Path;
use std::sync::Arc;

use ardur_fused_runtime::{
    ReconciliationAction, ReconciliationError, ReconciliationStrategy, load_persisted_chain,
    verify_persisted_chain,
};
use ardur_runtime::{ChatRuntime, SessionId};
use ardur_session_journals::{FileSessionJournal, JournalEntry, SessionJournal};

mod support;
use support::{
    EchoProvider, gate_holder, generous_budget, request_for, runtime_builder, valid_token,
};

/// Run `prompts.len()` clean turns through a runtime wired with a file journal
/// at `<root>/sessions/<session>/journal.jsonl` and a receipt log at
/// `receipt_log`, all on `session_id`. Returns once every turn has journaled.
async fn run_clean_turns(root: &Path, receipt_log: &Path, session_id: SessionId, prompts: &[&str]) {
    let provider = Arc::new(EchoProvider::new());
    let journal = Arc::new(FileSessionJournal::new(root, session_id).expect("journal opens"));
    let runtime = runtime_builder(provider.clone())
        .provision_budget(gate_holder(), generous_budget())
        .with_journal(journal)
        .receipt_log(receipt_log)
        .build()
        .expect("runtime builds");
    let token = valid_token();
    for prompt in prompts {
        runtime
            .submit(request_for(prompt, &token, session_id))
            .await
            .expect("clean turn completes");
    }
}

/// The path the file journal writes to for `session_id` under `root`.
fn journal_path(root: &Path, session_id: SessionId) -> std::path::PathBuf {
    root.join("sessions")
        .join(session_id.0.to_string())
        .join("journal.jsonl")
}

/// Drop the last `n` non-empty lines from a JSONL file on disk, simulating a
/// crash that committed the receipt but not the final journal append(s).
fn drop_last_journal_lines(path: &Path, n: usize) {
    let contents = std::fs::read_to_string(path).expect("journal readable");
    let mut lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
    for _ in 0..n {
        lines.pop();
    }
    let mut rewritten = lines.join("\n");
    if !rewritten.is_empty() {
        rewritten.push('\n');
    }
    std::fs::write(path, rewritten).expect("journal rewritable");
}

/// The `receipt_id`s the journal accounts for (via `AssistantMessage` entries).
fn journaled_ids(entries: &[JournalEntry]) -> Vec<uuid::Uuid> {
    entries
        .iter()
        .filter_map(|e| match e {
            JournalEntry::AssistantMessage { receipt_id, .. } => Some(receipt_id.0),
            _ => None,
        })
        .collect()
}

/// Build a fresh runtime over already-persisted paths, with a chosen strategy.
fn restart_over(
    root: &Path,
    receipt_log: &Path,
    session_id: SessionId,
    strategy: ReconciliationStrategy,
) -> (
    Arc<ardur_fused_runtime::FusedRuntime>,
    Arc<FileSessionJournal>,
    Arc<EchoProvider>,
) {
    let provider = Arc::new(EchoProvider::new());
    let journal = Arc::new(FileSessionJournal::new(root, session_id).expect("journal reopens"));
    let runtime = Arc::new(
        runtime_builder(provider.clone())
            .provision_budget(gate_holder(), generous_budget())
            .with_journal(journal.clone())
            .receipt_log(receipt_log)
            .reconciliation_strategy(strategy)
            .build()
            .expect("runtime rebuilds over persisted paths"),
    );
    (runtime, journal, provider)
}

#[tokio::test]
async fn reconcile_no_orphans_is_noop() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log = root.path().join("receipts.jsonl");
    let session_id = SessionId::new();

    run_clean_turns(root.path(), &receipt_log, session_id, &["one", "two"]).await;

    // No truncation: the journal accounts for both receipts.
    let (runtime, journal, _p) = restart_over(
        root.path(),
        &receipt_log,
        session_id,
        ReconciliationStrategy::AppendSyntheticJournal,
    );

    let before = journal.replay(session_id).await.expect("replay").len();
    let report = runtime
        .reconcile_receipts(false)
        .await
        .expect("reconcile succeeds");

    assert_eq!(report.receipt_count, 2);
    assert_eq!(report.orphan_receipt_count(), 0);
    assert_eq!(report.action, ReconciliationAction::NoOrphans);
    let after = journal.replay(session_id).await.expect("replay").len();
    assert_eq!(before, after, "a no-orphan sweep appends nothing");
}

#[tokio::test]
async fn reconcile_one_orphan_appends_synthetic_journal_entry() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log = root.path().join("receipts.jsonl");
    let session_id = SessionId::new();

    run_clean_turns(root.path(), &receipt_log, session_id, &["one", "two"]).await;

    // Crash residue: turn two's receipt is durable, but its assistant journal
    // entry never committed. Drop that one line.
    drop_last_journal_lines(&journal_path(root.path(), session_id), 1);

    let chain = load_persisted_chain(&receipt_log).expect("chain loads");
    assert_eq!(chain.len(), 2, "both receipts are durable");
    let orphan_id = chain[1].body.receipt_id;

    let (runtime, journal, _p) = restart_over(
        root.path(),
        &receipt_log,
        session_id,
        ReconciliationStrategy::AppendSyntheticJournal,
    );

    // Pre-state: exactly one orphan (turn two).
    let pre = journal.replay(session_id).await.expect("replay");
    assert!(
        !journaled_ids(&pre).contains(&orphan_id),
        "turn two is an orphan before reconciliation"
    );

    let report = runtime
        .reconcile_receipts(false)
        .await
        .expect("reconcile succeeds");
    assert_eq!(report.receipt_count, 2);
    assert_eq!(report.orphan_receipt_ids, vec![orphan_id]);
    assert_eq!(
        report.action,
        ReconciliationAction::AppendedSyntheticJournal { count: 1 }
    );

    // Post-state: the journal now accounts for the once-orphaned receipt, the
    // receipt log is untouched (the receipt is the source of truth), and a
    // recovery marker is visible in the entry's content.
    let post = journal.replay(session_id).await.expect("replay");
    assert!(
        journaled_ids(&post).contains(&orphan_id),
        "the orphan is now journaled by a recovery entry"
    );
    let recovered = post
        .iter()
        .rev()
        .find_map(|e| match e {
            JournalEntry::AssistantMessage {
                receipt_id,
                content,
                ..
            } if receipt_id.0 == orphan_id => Some(content.clone()),
            _ => None,
        })
        .expect("a recovery entry exists for the orphan");
    assert!(
        recovered.contains("reconciled") && recovered.contains(&orphan_id.to_string()),
        "the recovery entry is a visible marker naming the orphan, not a fabricated response"
    );
    assert_eq!(
        load_persisted_chain(&receipt_log)
            .expect("chain reloads")
            .len(),
        2,
        "AppendSyntheticJournal leaves the durable receipt chain untouched"
    );
}

#[tokio::test]
async fn reconcile_one_orphan_truncate_strategy_removes_receipt() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log = root.path().join("receipts.jsonl");
    let session_id = SessionId::new();

    run_clean_turns(root.path(), &receipt_log, session_id, &["one", "two"]).await;
    drop_last_journal_lines(&journal_path(root.path(), session_id), 1);

    let chain = load_persisted_chain(&receipt_log).expect("chain loads");
    assert_eq!(chain.len(), 2);
    let kept_id = chain[0].body.receipt_id;

    let (runtime, _journal, _p) = restart_over(
        root.path(),
        &receipt_log,
        session_id,
        ReconciliationStrategy::TruncateOrphans,
    );

    let report = runtime
        .reconcile_receipts(false)
        .await
        .expect("reconcile succeeds");
    assert_eq!(
        report.action,
        ReconciliationAction::TruncatedReceipts { count: 1 }
    );

    // The tail orphan is gone; the surviving prefix still verifies as a chain.
    let chain = load_persisted_chain(&receipt_log).expect("chain reloads");
    assert_eq!(chain.len(), 1, "the orphan suffix was truncated");
    assert_eq!(chain[0].body.receipt_id, kept_id, "turn one survives");
    verify_persisted_chain(&chain).expect("the truncated chain still verifies");

    // The chain tail was reset, so the next turn chains onto the retained tail
    // (not the removed orphan) and the chain stays linkable.
    runtime
        .submit(request_for("three", &valid_token(), session_id))
        .await
        .expect("post-truncation turn completes");
    let chain = load_persisted_chain(&receipt_log).expect("chain reloads after turn three");
    assert_eq!(
        chain.len(),
        2,
        "turn three appended onto the truncated chain"
    );
    verify_persisted_chain(&chain).expect("the chain verifies after a turn following truncation");
    assert_eq!(
        chain[1].body.parent_hash,
        Some(ardur_receipt::Sha256Digest::of(
            chain[0].jws_compact.as_bytes()
        )),
        "turn three chained onto the retained tail, not the removed orphan"
    );
}

#[tokio::test]
async fn reconcile_truncate_non_suffix_orphan_is_undecidable() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log = root.path().join("receipts.jsonl");
    let session_id = SessionId::new();

    run_clean_turns(
        root.path(),
        &receipt_log,
        session_id,
        &["one", "two", "three"],
    )
    .await;

    // Orphan the MIDDLE turn: drop turn two's assistant entry only (entries are
    // [u1,a1,u2,a2,u3,a3]; index 3 is a2). Turn three's receipt still chains
    // onto turn two's, so turn two's orphan is load-bearing — it is not a tail.
    let path = journal_path(root.path(), session_id);
    let contents = std::fs::read_to_string(&path).expect("journal readable");
    let mut lines: Vec<String> = contents
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    assert_eq!(lines.len(), 6, "three clean turns × (user + assistant)");
    lines.remove(3); // drop turn two's AssistantMessage
    std::fs::write(&path, lines.join("\n") + "\n").expect("journal rewritable");

    let (runtime, _journal, _p) = restart_over(
        root.path(),
        &receipt_log,
        session_id,
        ReconciliationStrategy::TruncateOrphans,
    );

    let err = runtime
        .reconcile_receipts(false)
        .await
        .expect_err("a non-suffix orphan cannot be truncated");
    assert!(
        matches!(err, ReconciliationError::Undecidable { .. }),
        "truncating a load-bearing middle orphan is undecidable, not a silent chain break: {err:?}"
    );

    // The store was not mutated: all three receipts remain and still verify.
    let chain = load_persisted_chain(&receipt_log).expect("chain reloads");
    assert_eq!(
        chain.len(),
        3,
        "no receipt was removed on the undecidable path"
    );
    verify_persisted_chain(&chain).expect("the chain is intact after the refusal");
}

#[tokio::test]
async fn reconcile_dry_run_reports_but_does_not_modify() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log = root.path().join("receipts.jsonl");
    let session_id = SessionId::new();

    run_clean_turns(root.path(), &receipt_log, session_id, &["one", "two"]).await;
    drop_last_journal_lines(&journal_path(root.path(), session_id), 1);

    let (runtime, journal, _p) = restart_over(
        root.path(),
        &receipt_log,
        session_id,
        ReconciliationStrategy::AppendSyntheticJournal,
    );

    let journal_before =
        std::fs::read_to_string(journal_path(root.path(), session_id)).expect("journal readable");
    let entries_before = journal.replay(session_id).await.expect("replay").len();

    let report = runtime
        .reconcile_receipts(true)
        .await
        .expect("dry-run reconcile succeeds");
    assert_eq!(report.orphan_receipt_count(), 1, "the orphan is detected");
    assert_eq!(
        report.action,
        ReconciliationAction::ReportedOnly,
        "a dry run reports without acting"
    );
    assert!(report.dry_run);

    let journal_after =
        std::fs::read_to_string(journal_path(root.path(), session_id)).expect("journal readable");
    let entries_after = journal.replay(session_id).await.expect("replay").len();
    assert_eq!(journal_before, journal_after, "dry run wrote nothing");
    assert_eq!(entries_before, entries_after);
}

#[tokio::test]
async fn reconcile_idempotent_on_repeat_runs() {
    let root = tempfile::tempdir().expect("tempdir");
    let receipt_log = root.path().join("receipts.jsonl");
    let session_id = SessionId::new();

    run_clean_turns(root.path(), &receipt_log, session_id, &["one", "two"]).await;
    drop_last_journal_lines(&journal_path(root.path(), session_id), 1);

    let (runtime, journal, _p) = restart_over(
        root.path(),
        &receipt_log,
        session_id,
        ReconciliationStrategy::AppendSyntheticJournal,
    );

    let first = runtime
        .reconcile_receipts(false)
        .await
        .expect("first reconcile");
    assert_eq!(
        first.action,
        ReconciliationAction::AppendedSyntheticJournal { count: 1 }
    );
    let after_first = journal.replay(session_id).await.expect("replay").len();

    // A second sweep sees the once-orphaned receipt now journaled (the recovery
    // entry is itself an AssistantMessage) and does nothing.
    let second = runtime
        .reconcile_receipts(false)
        .await
        .expect("second reconcile");
    assert_eq!(second.orphan_receipt_count(), 0);
    assert_eq!(second.action, ReconciliationAction::NoOrphans);
    let after_second = journal.replay(session_id).await.expect("replay").len();
    assert_eq!(
        after_first, after_second,
        "a repeat sweep appends no further recovery entries"
    );
}
