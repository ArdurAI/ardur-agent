//! Scenario §2.E9 — `session_journal_mid_turn_restart`.
//!
//! Kill a turn *mid-flight*, restart over the same on-disk state, replay, and
//! assert the receipt chain stays internally linkable across the crash — while
//! documenting, not papering over, the durability gap the crash exposes.
//!
//! # What the journal actually logs per turn (read first, designed around)
//!
//! The fused runtime's ten-stage pipeline (`crates/fused-runtime/src/runtime.rs`)
//! writes journal entries at **stage 10 only**, and exactly two of them per
//! successful turn:
//!
//! - [`JournalEntry::UserMessage`] (the prompt), then
//! - [`JournalEntry::AssistantMessage`] (the response, carrying the turn's
//!   `receipt_id`).
//!
//! There are **no** intermediate journal events — no `TurnStarted`, no
//! `CostReserved`, no `ProviderCalled`, no mid-turn `Checkpoint`. So this is
//! possibility *(a)* from the scenario brief: the journal records only terminal
//! turn state. The original backlog sketch (#9) imagined forcing a `Checkpoint`
//! mid-turn and calling `replay_from(checkpoint_id)`; the runtime never emits a
//! checkpoint during a turn, so that exact design is unrealizable today. The
//! meaningful crash-safety question is therefore the one the brief flags as the
//! *critical security property*: **can a crash leave an orphan receipt?**
//!
//! # The durability gap this test proves (and does NOT fake away)
//!
//! Stage ordering matters. Per turn:
//!
//! - **Stage 6** mints the receipt, advances the in-memory chain tail, and
//!   `persist_receipt` appends the compact JWS to the receipt log **with an
//!   `fsync` (`sync_all`)** — the receipt is durable here.
//! - **Stage 8** finalizes the cost reservation (refunds `reserved − actual`).
//! - **Stage 10** appends the journal entries (each fsynced), and is documented
//!   **non-fatal** — a journal append that errors is routed to `on_error` and
//!   the turn still returns `Ok`.
//!
//! A crash (or even a transient journal I/O error, given stage 10 is non-fatal)
//! in the window **after stage 6 and before stage 10 completes** leaves the
//! turn's receipt durably in the chain with **no journal `AssistantMessage`
//! referencing its `receipt_id`** — an *orphan receipt*. The receipt chain
//! itself stays hash-linkable (`verify_persisted_chain` passes; a later turn
//! chains onto the orphan's JWS with no `parent_hash` divergence), but the
//! journal under-counts turns relative to the receipt log, permanently.
//!
//! **This is a real Phase-3 durability gap**: the receipt log and the session
//! journal are two independent append-only stores committed in sequence, not
//! atomically, with the receipt committed strictly first. This test first
//! asserts what the pipeline does on such a crash (an orphan receipt survives),
//! then exercises the **ARD-17 remediation**: a fresh runtime's boot-time
//! [`reconcile_receipts`](ardur_fused_runtime::FusedRuntime::reconcile_receipts)
//! sweep detects the orphan and heals the journal (default
//! `AppendSyntheticJournal` strategy), so the "exactly one orphan" assertion of
//! the pre-fix world **inverts to zero orphans** once reconciliation runs. The
//! receipt chain — the source of truth — is left untouched; only the derived
//! journal view is repaired, with a visible recovery entry.
//!
//! # Crash-simulation approach (highest fidelity available)
//!
//! The brief ranks fault-injection via a stub `Provider` highest, but the
//! provider runs at **stage 5 — before** the receipt is minted, so a provider
//! fault can only model the clean-rollback case (no receipt, no journal entry),
//! not the orphan-receipt window. To inject a fault *inside* the stage-6→10
//! window we crash at the only seam that lives there: **the journal append at
//! stage 10**. A [`CrashAtAppend`] journal panics on the 3rd turn's first
//! append (after that turn's receipt is already minted, persisted, and
//! fsynced); the turn is driven on a `spawn_local` task so the panic surfaces as
//! a `JoinError` — a faithful "the process died mid-turn" rather than a tidy
//! `Result::Err`. Turns 1 and 2 journal cleanly through the same instance.
//!
//! 1. Runtime **A** runs two clean turns, then a third that crashes at the
//!    journal seam. Captured before the crash: receipt-chain tail, journal byte
//!    size, last entry id.
//! 2. On-disk post-crash state is asserted directly: 3 receipts (turn 3's is
//!    durable), 4 journal entries (turn 3 absent) — the orphan. The cost
//!    reservation for turn 3 was finalized at stage 8 *before* the stage-10
//!    crash, so the budget shows **no leaked reservation**.
//! 3. Runtime **B** is built over the same receipt-log + journal paths. It
//!    replays both clean turns intact and exposes exactly one orphan receipt,
//!    then its boot-time reconciliation sweep heals the journal so zero orphans
//!    remain. A fourth turn chains onto the reconciled turn-three JWS with no
//!    divergence — `verify_persisted_chain` passes over all four, and the
//!    journal now accounts for every receipt in the chain.
//!
//! This is the last of the nine §2.E scenarios in
//! `architect/backlog/e2e-test-coverage-gaps.md`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use ardur_e2e_tests::fixtures;
use ardur_fused_runtime::{ReconciliationAction, load_persisted_chain, verify_persisted_chain};
use ardur_receipt::Sha256Digest;
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, SessionId, SubmitRequest};
use ardur_session_journals::{
    EntryId, FileSessionJournal, JournalEntry, JournalError, SessionJournal,
};

mod support;
use support::EchoProvider;

/// The 0-based journal-append index at which the simulated crash fires. Each
/// clean turn appends two entries (user + assistant), so turns one and two
/// consume appends 0..=3 and the third turn's *first* append is index 4 — the
/// moment to die, with that turn's receipt already minted, persisted, and
/// fsynced one stage earlier.
const CRASH_AT_APPEND: u64 = 4;

/// A [`SessionJournal`] decorator that delegates every call to an inner
/// [`FileSessionJournal`] but **panics** on the append whose 0-based index
/// equals `crash_at` — simulating a process killed mid-turn at the stage-10
/// journal seam, after the turn's receipt was already durably persisted.
struct CrashAtAppend {
    inner: Arc<FileSessionJournal>,
    seen: AtomicU64,
    crash_at: u64,
}

impl CrashAtAppend {
    fn new(inner: Arc<FileSessionJournal>, crash_at: u64) -> Self {
        Self {
            inner,
            seen: AtomicU64::new(0),
            crash_at,
        }
    }

    /// The on-disk path of the wrapped journal — for direct file inspection.
    fn journal_path(&self) -> &Path {
        self.inner.path()
    }
}

#[async_trait]
impl SessionJournal for CrashAtAppend {
    async fn append(&self, entry: JournalEntry) -> Result<EntryId, JournalError> {
        let idx = self.seen.fetch_add(1, Ordering::SeqCst);
        if idx == self.crash_at {
            // Crash *before* delegating, so the inner file is never touched for
            // this entry — the journal has no record of the turn, exactly as a
            // power loss between the receipt fsync and the journal fsync would
            // leave it.
            panic!(
                "simulated mid-turn crash at journal append #{idx}: process killed after the \
                 turn's receipt was minted, chained, persisted, and fsynced (stage 6) but before \
                 the journal entry was committed (stage 10)"
            );
        }
        self.inner.append(entry).await
    }

    async fn replay(&self, session_id: SessionId) -> Result<Vec<JournalEntry>, JournalError> {
        self.inner.replay(session_id).await
    }

    async fn replay_from(
        &self,
        session_id: SessionId,
        from: EntryId,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        self.inner.replay_from(session_id, from).await
    }

    async fn close(&self) -> Result<(), JournalError> {
        self.inner.close().await
    }

    fn session_id(&self) -> &SessionId {
        self.inner.session_id()
    }
}

/// The `receipt_id`s carried by every `AssistantMessage` in a replayed journal —
/// the set of turns the journal can actually account for.
fn journaled_receipt_ids(entries: &[JournalEntry]) -> Vec<uuid::Uuid> {
    entries
        .iter()
        .filter_map(|e| match e {
            JournalEntry::AssistantMessage { receipt_id, .. } => Some(receipt_id.0),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn journal_mid_turn_crash_orphan_receipt_is_reconciled_at_boot() {
    let root = fixtures::temp_session_root();
    let receipt_log = root.path().join("receipts.jsonl");
    let session_id = SessionId::new();
    let token = fixtures::dev_valid_cap_token();

    let request = |content: &str| SubmitRequest {
        messages: vec![ChatMessage::user(content)],
        cap_token: CapTokenRef(token.clone()),
        session_id,
        requested_provider: None,
    };

    // ---- 1. Runtime A: two clean turns, then a third that dies at the journal.
    //
    // Everything for Runtime A lives inside a `LocalSet` so the crashing third
    // turn can be driven on a `spawn_local` task whose panic we observe as a
    // `JoinError` — the faithful "process died mid-turn" signal.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let provider = Arc::new(EchoProvider::new());
            let file_journal = Arc::new(
                FileSessionJournal::new(root.path(), session_id).expect("journal A opens"),
            );
            let crashing = Arc::new(CrashAtAppend::new(file_journal, CRASH_AT_APPEND));
            let runtime_a = Arc::new(
                fixtures::fused_builder(provider.clone())
                    .with_journal(crashing.clone())
                    .receipt_log(&receipt_log)
                    .build()
                    .expect("runtime A wires"),
            );

            // Two turns journal cleanly.
            for prompt in ["turn one", "turn two"] {
                runtime_a
                    .submit(request(prompt))
                    .await
                    .expect("each pre-crash turn completes");
            }
            assert_eq!(provider.call_count(), 2, "A dispatched the two clean turns");

            // Snapshot the durable + in-memory state at the point of no return.
            let chain = load_persisted_chain(&receipt_log).expect("chain loads");
            assert_eq!(chain.len(), 2, "two receipts before the crash");
            verify_persisted_chain(&chain).expect("the pre-crash chain verifies");
            let pre_crash_tail = Sha256Digest::of(chain[1].jws_compact.as_bytes());

            let replayed = crashing
                .replay(session_id)
                .await
                .expect("journal A replays");
            assert_eq!(replayed.len(), 4, "two turns × (user + assistant)");
            let pre_crash_last_entry = EntryId::new(replayed.len() as u64 - 1);
            let pre_crash_journal_size = std::fs::metadata(crashing.journal_path())
                .expect("journal file exists")
                .len();
            assert!(pre_crash_journal_size > 0, "the journal has bytes on disk");

            // ---- 2. The third turn crashes at the stage-10 journal append.
            //
            // Suppress the intended panic's backtrace so it does not pollute test
            // output; this binary's only panic is this deliberate one.
            let prev_hook = std::panic::take_hook();
            std::panic::set_hook(Box::new(|_| {}));
            let rt = runtime_a.clone();
            let req3 = request("turn three");
            let handle = tokio::task::spawn_local(async move { rt.submit(req3).await });
            let outcome = handle.await;
            std::panic::set_hook(prev_hook);

            let join_err = outcome.expect_err("the third turn's task must not complete normally");
            assert!(
                join_err.is_panic(),
                "the mid-turn kill surfaces as a panicked task, not a cancellation"
            );
            assert_eq!(
                provider.call_count(),
                3,
                "the provider was called for turn three before the crash"
            );

            // ---- On-disk state immediately after the crash. ----

            // The third turn's receipt IS durable — persisted+fsynced at stage 6,
            // before the stage-10 crash.
            let chain = load_persisted_chain(&receipt_log).expect("chain reloads post-crash");
            assert_eq!(
                chain.len(),
                3,
                "turn three's receipt survived the crash (stage 6 precedes the journal)"
            );
            verify_persisted_chain(&chain)
                .expect("the receipt chain is still internally linkable after the crash");
            assert_eq!(
                chain[2].body.parent_hash,
                Some(pre_crash_tail),
                "turn three's receipt chained onto the pre-crash tail before the crash"
            );

            // The journal did NOT record turn three — its append is exactly where
            // we died. The file is byte-for-byte what it was before the crash.
            let replayed = crashing
                .replay(session_id)
                .await
                .expect("journal A replays");
            assert_eq!(
                replayed.len(),
                4,
                "turn three left NO journal entry — the crash was at its first append"
            );
            assert_eq!(
                pre_crash_last_entry,
                EntryId::new(3),
                "the last durable entry id is unchanged across the crash"
            );
            let post_crash_journal_size = std::fs::metadata(crashing.journal_path())
                .expect("journal file exists")
                .len();
            assert_eq!(
                post_crash_journal_size, pre_crash_journal_size,
                "not one byte was appended to the journal for the crashed turn"
            );

            // THE GAP: 3 receipts on disk, only 2 turns journaled → 1 orphan.
            let journaled = journaled_receipt_ids(&replayed);
            assert_eq!(journaled.len(), 2, "the journal accounts for two turns");
            let orphans = chain
                .iter()
                .filter(|r| !journaled.contains(&r.body.receipt_id))
                .count();
            assert_eq!(
                orphans, 1,
                "turn three's receipt is an orphan: durable in the chain, absent from the journal"
            );

            // No leaked cost reservation: stage 8 finalized turn three's hold
            // BEFORE the stage-10 crash, so the budget is whole. (EchoProvider
            // bills zero, so every reserved-then-finalized turn nets to zero and
            // the balance returns to its provisioned value; a stranded hold would
            // show the envelope still deducted.)
            let remaining = runtime_a
                .remaining_budget(&fixtures::gate_holder())
                .await
                .expect("the holder's budget is readable");
            assert_eq!(
                remaining.cents, 1_000_000,
                "turn three's reservation was finalized, not stranded — no leaked budget"
            );
        })
        .await;

    // ---- 3. Runtime B over the SAME paths: replay, prove the gap, then
    //         RECONCILE it away (ARD-17). ----
    //
    // A real boot would call `FusedRuntimeBuilder::build_reconciled().await` to
    // build-and-heal in one step; here we build, *prove the pre-reconciliation
    // orphan still exists* (the durability gap this scenario documents), and
    // then call `reconcile_receipts` explicitly so the recovery is legible as
    // its own step. The builder's default strategy is `AppendSyntheticJournal`,
    // so no strategy is set.
    let provider_b = Arc::new(EchoProvider::new());
    let journal_b =
        Arc::new(FileSessionJournal::new(root.path(), session_id).expect("journal B reopens"));
    let runtime_b = fixtures::fused_builder(provider_b.clone())
        .with_journal(journal_b.clone())
        .receipt_log(&receipt_log)
        .build()
        .expect("runtime B wires over the persisted paths");

    // The two clean turns are intact in BOTH stores, and cross-reference: each
    // journaled assistant entry names a receipt that is actually in the chain.
    let chain = load_persisted_chain(&receipt_log).expect("chain loads on restart");
    assert_eq!(chain.len(), 3, "restart sees the three persisted receipts");
    verify_persisted_chain(&chain).expect("the chain verifies on restart");

    let replayed = journal_b
        .replay(session_id)
        .await
        .expect("journal B replays");
    assert_eq!(replayed.len(), 4, "restart replays the two clean turns");
    let journaled = journaled_receipt_ids(&replayed);
    assert_eq!(
        journaled,
        vec![chain[0].body.receipt_id, chain[1].body.receipt_id],
        "the journaled turns reference receipts one and two, in order"
    );

    // PRE-RECONCILIATION: the orphan survives the restart — turn three's receipt
    // is in the chain but no journal entry accounts for it. This is the gap.
    let orphan_id = chain[2].body.receipt_id;
    assert!(
        !journaled.contains(&orphan_id),
        "turn three's receipt is an orphan at boot — the journal never recorded it"
    );

    // ---- ARD-17 reconciliation sweep. The "exactly one orphan" assertion of
    //      the pre-fix world now INVERTS: after the sweep, the journal accounts
    //      for turn three's receipt too, so zero orphans remain. The durable
    //      receipt chain is left untouched (it is the source of truth); only the
    //      journal is healed, with a *visible* recovery entry.
    let report = runtime_b
        .reconcile_receipts(false)
        .await
        .expect("reconciliation succeeds at boot");
    assert_eq!(report.receipt_count, 3);
    assert_eq!(
        report.orphan_receipt_ids,
        vec![orphan_id],
        "the sweep detects exactly the turn-three orphan"
    );
    assert_eq!(
        report.action,
        ReconciliationAction::AppendedSyntheticJournal { count: 1 },
        "the default strategy heals the journal rather than destroying the receipt"
    );

    // The receipt chain is unchanged; the journal gained one recovery entry that
    // closes the gap — zero orphans now.
    let chain = load_persisted_chain(&receipt_log).expect("chain reloads post-reconcile");
    assert_eq!(
        chain.len(),
        3,
        "reconciliation did not touch the receipt log"
    );
    let replayed = journal_b
        .replay(session_id)
        .await
        .expect("journal B replays post-reconcile");
    assert_eq!(
        replayed.len(),
        5,
        "two clean turns (4 entries) + one recovery entry for the orphan"
    );
    let journaled = journaled_receipt_ids(&replayed);
    let orphans = chain
        .iter()
        .filter(|r| !journaled.contains(&r.body.receipt_id))
        .count();
    assert_eq!(
        orphans, 0,
        "ZERO orphans after reconciliation — turn three's receipt is now journaled (the inversion)"
    );

    // ---- 4. A fourth turn recovers cleanly, chaining onto turn three's JWS. ----
    runtime_b
        .submit(request("turn four"))
        .await
        .expect("the post-restart turn completes");
    assert_eq!(provider_b.call_count(), 1, "B dispatched the fourth turn");

    let chain = load_persisted_chain(&receipt_log).expect("chain reloads after turn four");
    assert_eq!(
        chain.len(),
        4,
        "the restart appended, not restarted, the chain"
    );
    assert!(chain[0].body.parent_hash.is_none(), "genesis unchanged");
    assert_eq!(
        chain[3].body.parent_hash,
        Some(Sha256Digest::of(chain[2].jws_compact.as_bytes())),
        "turn four chains onto the (now-reconciled) turn-three receipt — no parent_hash divergence"
    );
    verify_persisted_chain(&chain)
        .expect("the full four-receipt chain verifies across the restart");

    // The journal now accounts for ALL four receipts: turns one, two, four, and
    // the recovered turn three. The receipt log (4) and the journaled turns (4)
    // agree — the gap is closed, permanently.
    let replayed = journal_b
        .replay(session_id)
        .await
        .expect("journal B replays after turn four");
    assert_eq!(
        replayed.len(),
        7,
        "4 clean-turn entries + 1 recovery entry + 2 for turn four"
    );
    let journaled = journaled_receipt_ids(&replayed);
    let orphans = chain
        .iter()
        .filter(|r| !journaled.contains(&r.body.receipt_id))
        .count();
    assert_eq!(
        orphans, 0,
        "zero orphans remain: every receipt in the chain is accounted for by the journal"
    );
    assert!(
        journaled.contains(&orphan_id),
        "the once-orphaned turn-three receipt stays accounted for after the fourth turn"
    );

    drop(root);
}
