//! gh#497 — claim-once execution claims and single-winner decisions.
//!
//! These tests pin the contract that replaces the old read/check/rename
//! store: one transactional transition path with exactly one durable winner
//! across independent handles AND processes, claim bound to the approved
//! action/arguments/session, no resurrection of spent cards, explicit
//! ambiguous-effect state, and durable audit obligations.
//!
//! The pre-fix RED evidence (the old `consume` returning `Ok` for a spent
//! card, and a double-`Ok` conflicting decide on attempt 0) is recorded in
//! the PR; the fault-injection evidence that these guards can fail is
//! archived alongside it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use ardur_approvals::{
    ApprovalStatus, ApprovalStore, ApprovalStoreError, ClaimBinding, ClaimOutcome, Decision,
    EffectState, InvocationOutcome, InvocationResult,
};

fn store() -> (tempfile::TempDir, ApprovalStore) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = ApprovalStore::new(dir.path().join("approvals"));
    (dir, store)
}

fn approved_card(store: &ApprovalStore) -> String {
    let card = store
        .propose(
            "shell.run",
            "shell.exec",
            "deadbeef",
            Some("sess-1".to_string()),
            "r",
            1,
        )
        .expect("propose succeeds");
    let id = card.id.clone().expect("id injected");
    store
        .decide(&id, Decision::Approve, 2)
        .expect("decide succeeds");
    id
}

fn binding<'a>(tool: &'a str, digest: &'a str, session: Option<&'a str>) -> ClaimBinding<'a> {
    ClaimBinding {
        tool,
        arguments_digest: digest,
        session_id: session,
        claimed_by: Some("ardur:test-operator"),
    }
}

// ---------------------------------------------------------------------------
// Claim-once
// ---------------------------------------------------------------------------

/// The fresh-card success control: the first claim wins and records the
/// binding's caller identity on the card.
#[test]
fn a_fresh_approved_card_is_claimed_once() {
    let (_dir, store) = store();
    let id = approved_card(&store);

    let outcome = store
        .claim_execution(&id, &binding("shell.run", "deadbeef", Some("sess-1")), 42)
        .expect("the first claim succeeds");
    assert!(outcome.is_won(), "the first claim must grant the execution");
    let card = outcome.into_card();
    assert_eq!(card.status, ApprovalStatus::Consumed);
    assert_eq!(card.consumed_at, Some(42));
    assert_eq!(card.consumed_by.as_deref(), Some("ardur:test-operator"));

    // Durable: a fresh handle observes the spent card with its attribution.
    let reread = store.read(&id).expect("card re-readable");
    assert_eq!(reread.status, ApprovalStatus::Consumed);
    assert_eq!(reread.consumed_by.as_deref(), Some("ardur:test-operator"));
}

/// The repeated-use refusal: a second claim is an observation, never a fresh
/// grant — sequentially and by identity.
#[test]
fn a_second_claim_is_an_observation_not_a_fresh_execution_claim() {
    let (_dir, store) = store();
    let id = approved_card(&store);

    let first = store
        .claim_execution(&id, &binding("shell.run", "deadbeef", Some("sess-1")), 42)
        .expect("the first claim succeeds");
    assert!(first.is_won());

    let second = store
        .claim_execution(&id, &binding("shell.run", "deadbeef", Some("sess-1")), 43)
        .expect("the second claim is observable");
    assert!(
        matches!(second, ClaimOutcome::AlreadySpent(_)),
        "an already-spent approval must not grant a new execution: {second:?}"
    );
    // The observation carries the ORIGINAL claim's stamp, not the retry's.
    let observed = second.into_card();
    assert_eq!(observed.consumed_at, Some(42));
}

/// Two independent handles claiming behind a barrier: exactly one wins, and
/// an effect performed iff `Won` happens exactly once.
#[test]
fn overlapping_claims_have_exactly_one_winner_and_one_effect() {
    let (_dir, store) = store();
    let id = approved_card(&store);

    let barrier = Arc::new(Barrier::new(2));
    let effects = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let handle = ApprovalStore::new(store.dir().to_path_buf());
        let id = id.clone();
        let barrier = Arc::clone(&barrier);
        let effects = Arc::clone(&effects);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            let outcome = handle
                .claim_execution(&id, &binding("shell.run", "deadbeef", Some("sess-1")), 42)
                .expect("claims are observable");
            if outcome.is_won() {
                // The effect: performed only on a won claim.
                effects.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    for handle in handles {
        handle.join().expect("claimer thread");
    }
    assert_eq!(
        effects.load(Ordering::SeqCst),
        1,
        "exactly one overlapping claim may produce an effect"
    );
}

/// A pending or denied card can never be claimed.
#[test]
fn a_pending_or_denied_card_cannot_be_claimed() {
    let (_dir, store) = store();
    let pending = store
        .propose("t", "c", "d", None, "r", 1)
        .expect("propose succeeds");
    let pending_id = pending.id.clone().unwrap();
    let err = store
        .claim_execution(&pending_id, &binding("t", "d", None), 2)
        .expect_err("a pending card is not a grant");
    assert!(matches!(
        err,
        ApprovalStoreError::NotApproved(ApprovalStatus::Pending)
    ));

    store
        .decide(
            &pending_id,
            Decision::Reject {
                reason: "no".to_string(),
            },
            3,
        )
        .expect("decide succeeds");
    let err = store
        .claim_execution(&pending_id, &binding("t", "d", None), 4)
        .expect_err("a denied card is not a grant");
    assert!(matches!(
        err,
        ApprovalStoreError::NotApproved(ApprovalStatus::Denied)
    ));
}

// ---------------------------------------------------------------------------
// Binding
// ---------------------------------------------------------------------------

/// A claim for a different tool/arguments/session than the card records is
/// refused and leaves the card unspent.
#[test]
fn a_claim_must_match_the_approved_action_arguments_and_session() {
    let (_dir, store) = store();
    let id = approved_card(&store);

    for (field, bad) in [
        ("tool", binding("file.write", "deadbeef", Some("sess-1"))),
        (
            "arguments_digest",
            binding("shell.run", "cafe", Some("sess-1")),
        ),
        (
            "session_id",
            binding("shell.run", "deadbeef", Some("sess-2")),
        ),
        ("session_id", binding("shell.run", "deadbeef", None)),
    ] {
        let err = store
            .claim_execution(&id, &bad, 42)
            .expect_err("a mismatched claim must fail closed");
        assert!(
            matches!(err, ApprovalStoreError::BindingMismatch(f) if f == field),
            "expected BindingMismatch({field}), got {err:?}"
        );
        assert_eq!(
            store.read(&id).expect("card readable").status,
            ApprovalStatus::Approved,
            "a refused claim must not spend the card"
        );
    }
}

/// The explicit legacy treatment: a decide-half-era card (no
/// tool/arguments/session fields) can be decided but never claimed.
#[test]
fn a_legacy_card_without_binding_fields_can_be_decided_but_never_claimed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let approvals = dir.path().join("approvals");
    std::fs::create_dir_all(&approvals).expect("approvals dir");
    // The PR #279 seed shape: id/status/summary and nothing else.
    std::fs::write(
        approvals.join("legacy-1.json"),
        serde_json::json!({
            "id": "legacy-1",
            "status": "pending",
            "summary": "delete the production database"
        })
        .to_string(),
    )
    .expect("seed legacy card");

    let store = ApprovalStore::new(&approvals);
    // Decision still works (the open schema preserves `summary`).
    let decided = store
        .decide("legacy-1", Decision::Approve, 10)
        .expect("a legacy card is decidable");
    assert_eq!(decided.status, ApprovalStatus::Approved);
    assert_eq!(
        decided.extra.get("summary").and_then(|v| v.as_str()),
        Some("delete the production database"),
        "producer-defined fields survive the typed round-trip"
    );

    let err = store
        .claim_execution(
            "legacy-1",
            &binding("shell.run", "deadbeef", Some("sess-1")),
            11,
        )
        .expect_err("a card that predates binding fields grants nothing");
    assert!(matches!(err, ApprovalStoreError::BindingMismatch("tool")));
    assert_eq!(
        store.read("legacy-1").expect("card readable").status,
        ApprovalStatus::Approved,
        "the refused claim leaves the legacy card decided, unspent"
    );
}

/// Review P1: an EMPTY binding must never match a card whose fields are
/// absent (legacy) — absence is not equality. Also covers an empty binding
/// presented against a fully-bound card.
#[test]
fn an_empty_binding_never_wins_a_claim() {
    let dir = tempfile::tempdir().expect("temp dir");
    let approvals = dir.path().join("approvals");
    std::fs::create_dir_all(&approvals).expect("approvals dir");
    std::fs::write(
        approvals.join("legacy-2.json"),
        serde_json::json!({
            "id": "legacy-2",
            "status": "approved",
            "summary": "approved before binding fields existed"
        })
        .to_string(),
    )
    .expect("seed approved legacy card");

    let legacy_store = ApprovalStore::new(&approvals);
    // The bypass shape: every binding field empty, exactly what a legacy card
    // deserializes to. This must NOT grant Won.
    let err = legacy_store
        .claim_execution("legacy-2", &binding("", "", None), 11)
        .expect_err("an empty binding on an empty card is not a match");
    assert!(matches!(err, ApprovalStoreError::BindingMismatch("tool")));
    assert_eq!(
        legacy_store.read("legacy-2").expect("card readable").status,
        ApprovalStatus::Approved,
        "the refused claim leaves the card unspent"
    );

    // And an empty binding field against a fully-bound card fails too.
    let (_dir2, store2) = store();
    let id = approved_card(&store2);
    for bad in [
        binding("", "deadbeef", Some("sess-1")),
        binding("shell.run", "", Some("sess-1")),
    ] {
        assert!(
            matches!(
                store2.claim_execution(&id, &bad, 12),
                Err(ApprovalStoreError::BindingMismatch(_))
            ),
            "an empty binding field must fail closed: {bad:?}"
        );
        assert_eq!(
            store2.read(&id).expect("card readable").status,
            ApprovalStatus::Approved
        );
    }
}

/// Review P2: a legacy card keeps its original shape through a decision —
/// deciding must not invent `created_at`/`tool`/`capability`/
/// `arguments_digest`/`reason` fields it never had.
#[test]
fn a_legacy_card_keeps_its_original_shape_through_a_decision() {
    let dir = tempfile::tempdir().expect("temp dir");
    let approvals = dir.path().join("approvals");
    std::fs::create_dir_all(&approvals).expect("approvals dir");
    let path = approvals.join("legacy-3.json");
    std::fs::write(
        &path,
        serde_json::json!({
            "id": "legacy-3",
            "status": "pending",
            "summary": "shrink the fleet"
        })
        .to_string(),
    )
    .expect("seed legacy card");

    let store = ApprovalStore::new(&approvals);
    store
        .decide("legacy-3", Decision::Approve, 10)
        .expect("a legacy card is decidable");

    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("card readable"))
            .expect("card parses");
    let object = raw.as_object().expect("card is an object");
    for absent in [
        "tool",
        "capability",
        "arguments_digest",
        "reason",
        "created_at",
    ] {
        assert!(
            !object.contains_key(absent),
            "deciding must not invent a `{absent}` field on a legacy card: {raw}"
        );
    }
    assert_eq!(raw["status"], "approved");
    assert!(raw["decided_at"].is_u64());
    assert_eq!(raw["summary"], "shrink the fleet");
    // The decision carries its audit obligation until the caller settles it.
    assert_eq!(raw["audit_pending"], true);
}

/// The decision stamps its audit obligation in the same write as the
/// decision itself: a card read between decide and any audit attempt already
/// reports the obligation.
#[test]
fn a_decision_stamps_its_audit_obligation_atomically() {
    let (_dir, store) = store();
    let card = store.propose("t", "c", "d", None, "r", 1).unwrap();
    let id = card.id.clone().unwrap();

    let decided = store.decide(&id, Decision::Approve, 2).unwrap();
    assert_eq!(decided.audit_pending, Some(true));
    assert!(decided.audit_pending_reason.is_some());

    // A fresh handle (post-crash observer) sees the same obligation.
    let observer = ApprovalStore::new(store.dir().to_path_buf());
    let observed = observer.read(&id).expect("obligation is durable");
    assert_eq!(observed.audit_pending, Some(true));
}

/// Read and write failures stay distinguishable: a card that cannot be read
/// reports `Read`; a store that cannot persist reports `Write`.
#[test]
fn read_and_write_failures_are_distinct() {
    use std::os::unix::fs::PermissionsExt;

    let (_dir, store) = store();
    let card = store.propose("t", "c", "d", None, "r", 1).unwrap();
    let id = card.id.clone().unwrap();
    let path = store.dir().join(format!("{id}.json"));

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
        .expect("chmod the card unreadable");
    let err = store.read(&id).expect_err("an unreadable card errors");
    assert!(
        matches!(err, ApprovalStoreError::Read(_)),
        "expected a Read error, got {err:?}"
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("restore the card");

    // A read-only store directory fails the persist side (the lock file
    // cannot be created).
    let mut perms = std::fs::metadata(store.dir())
        .expect("dir metadata")
        .permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(store.dir(), perms.clone()).expect("chmod the dir read-only");
    let err = store
        .decide(&id, Decision::Approve, 2)
        .expect_err("a read-only store cannot persist");
    assert!(
        matches!(err, ApprovalStoreError::Write(_)),
        "expected a Write error, got {err:?}"
    );
    perms.set_mode(0o755);
    std::fs::set_permissions(store.dir(), perms).expect("restore the dir");
}

// ---------------------------------------------------------------------------
// Single-winner decisions
// ---------------------------------------------------------------------------

/// Two independent handles racing an approve and a reject over the same
/// pending card produce exactly one `Ok`, one explicit conflict, and a
/// durable record matching the winner — on every attempt.
#[test]
fn concurrent_conflicting_decisions_have_exactly_one_durable_winner() {
    for attempt in 0..25 {
        let (_dir, store) = store();
        let card = store
            .propose("shell.run", "shell.exec", "deadbeef", None, "r", 1)
            .expect("propose succeeds");
        let id = card.id.clone().expect("id injected");

        let handle_a = ApprovalStore::new(store.dir().to_path_buf());
        let handle_b = ApprovalStore::new(store.dir().to_path_buf());
        let barrier = Arc::new(Barrier::new(2));
        let id_a = id.clone();
        let id_b = id.clone();

        let barrier_a = Arc::clone(&barrier);
        let approve = std::thread::spawn(move || {
            barrier_a.wait();
            handle_a.decide(&id_a, Decision::Approve, 10)
        });
        let barrier_b = Arc::clone(&barrier);
        let reject = std::thread::spawn(move || {
            barrier_b.wait();
            handle_b.decide(
                &id_b,
                Decision::Reject {
                    reason: "no".to_string(),
                },
                10,
            )
        });

        let approved = approve.join().expect("approve thread");
        let rejected = reject.join().expect("reject thread");
        let winners = approved.is_ok() as u32 + rejected.is_ok() as u32;
        assert_eq!(
            winners, 1,
            "attempt {attempt}: exactly one decision may win, got approve={approved:?} reject={rejected:?}"
        );
        let loser = if approved.is_ok() {
            &rejected
        } else {
            &approved
        };
        assert!(
            matches!(loser, Err(ApprovalStoreError::AlreadyDecided)),
            "attempt {attempt}: the loser must get an explicit conflict, got {loser:?}"
        );

        let durable = store.read(&id).expect("card readable");
        if approved.is_ok() {
            assert_eq!(durable.status, ApprovalStatus::Approved);
        } else {
            assert_eq!(durable.status, ApprovalStatus::Denied);
            assert_eq!(durable.deny_reason.as_deref(), Some("no"));
        }
        let reread = store.read(&id).expect("card re-readable");
        assert_eq!(
            reread.status, durable.status,
            "attempt {attempt}: the losing write must not flip the durable record"
        );
    }
}

/// A stale decision can never resurrect a consumed card.
#[test]
fn a_stale_decision_cannot_resurrect_a_consumed_card() {
    let (_dir, store) = store();
    let id = approved_card(&store);
    store
        .claim_execution(&id, &binding("shell.run", "deadbeef", Some("sess-1")), 5)
        .expect("claim succeeds");

    // A decision writer that observed `pending` long ago finally writes: the
    // store refuses — consumed is decided.
    let stale = store.decide(&id, Decision::Approve, 99);
    assert!(matches!(stale, Err(ApprovalStoreError::AlreadyDecided)));
    let stale_reject = store.decide(
        &id,
        Decision::Reject {
            reason: "late".to_string(),
        },
        99,
    );
    assert!(matches!(
        stale_reject,
        Err(ApprovalStoreError::AlreadyDecided)
    ));
    assert_eq!(
        store.read(&id).expect("card readable").status,
        ApprovalStatus::Consumed,
        "the spent card stays spent"
    );
}

/// propose_if_absent is one transaction: racing identical first calls mint
/// exactly one pending card, and the loser observes it.
#[test]
fn overlapping_proposes_mint_one_card() {
    let (_dir, store) = store();
    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let handle = ApprovalStore::new(store.dir().to_path_buf());
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            handle
                .propose_if_absent(
                    "shell.run",
                    "shell.exec",
                    "deadbeef",
                    Some("sess-1".to_string()),
                    "r",
                    1,
                )
                .expect("propose succeeds")
        }));
    }
    let results: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().expect("propose thread"))
        .collect();
    let created = results.iter().filter(|(_, created)| *created).count();
    assert_eq!(created, 1, "exactly one racer creates the card");
    assert_eq!(
        results[0].0.id, results[1].0.id,
        "both racers observe the same card"
    );
    assert_eq!(store.list().expect("list succeeds").len(), 1);
}

// ---------------------------------------------------------------------------
// Effect state and invocation outcomes
// ---------------------------------------------------------------------------

/// The ambiguous-effect contract: claimed-but-unrecorded reads as ambiguous;
/// a recorded outcome reads as recorded; an unclaimed card is not spent.
#[test]
fn effect_state_distinguishes_unspent_ambiguous_and_recorded() {
    let (_dir, store) = store();
    let id = approved_card(&store);

    assert_eq!(
        store.effect_state(&id).expect("state loads"),
        EffectState::NotSpent,
        "an approved-but-unclaimed card authorized nothing yet"
    );

    store
        .claim_execution(&id, &binding("shell.run", "deadbeef", Some("sess-1")), 5)
        .expect("claim succeeds");
    assert_eq!(
        store.effect_state(&id).expect("state loads"),
        EffectState::Ambiguous,
        "claimed without a recorded outcome is the explicit unknown-effect state"
    );

    store
        .record_invocation_outcome(
            &id,
            InvocationOutcome {
                result: InvocationResult::Failed,
                finished_at: 6,
            },
        )
        .expect("the outcome records");
    assert_eq!(
        store.effect_state(&id).expect("state loads"),
        EffectState::Recorded(InvocationOutcome {
            result: InvocationResult::Failed,
            finished_at: 6,
        }),
        "a failed call is recorded as failed — never as success"
    );
}

/// An outcome can only be recorded for a claimed card.
#[test]
fn recording_an_outcome_requires_a_spent_card() {
    let (_dir, store) = store();
    let card = store.propose("t", "c", "d", None, "r", 1).unwrap();
    let id = card.id.clone().unwrap();
    let err = store
        .record_invocation_outcome(
            &id,
            InvocationOutcome {
                result: InvocationResult::Completed,
                finished_at: 2,
            },
        )
        .expect_err("an unclaimed card has no invocation to record");
    assert!(matches!(
        err,
        ApprovalStoreError::NotConsumed(ApprovalStatus::Pending)
    ));
}

// ---------------------------------------------------------------------------
// Audit obligations
// ---------------------------------------------------------------------------

/// The durable pending-audit obligation: marked on a decided card, cleared
/// by the receipt link, refused on a pending card.
#[test]
fn audit_obligations_are_durable_and_recoverable() {
    let (_dir, store) = store();
    let card = store.propose("t", "c", "d", None, "r", 1).unwrap();
    let id = card.id.clone().unwrap();

    let err = store
        .mark_audit_pending(&id, "receipt: worker gone")
        .expect_err("a pending card can carry no decision audit obligation");
    assert!(matches!(err, ApprovalStoreError::NotDecided));

    store.decide(&id, Decision::Approve, 2).unwrap();
    store
        .mark_audit_pending(&id, "receipt: worker gone")
        .expect("the obligation records");
    let marked = store.read(&id).expect("card readable");
    assert_eq!(marked.audit_pending, Some(true));
    assert_eq!(
        marked.audit_pending_reason.as_deref(),
        Some("receipt: worker gone")
    );

    // A fresh handle (e.g. a recovery pass after restart) observes it and
    // settles it with the minted receipt.
    let recovery = ApprovalStore::new(store.dir().to_path_buf());
    let marked = recovery
        .read(&id)
        .expect("a fresh handle sees the obligation");
    assert_eq!(marked.audit_pending, Some(true));
    recovery
        .record_audit_receipt(&id, "receipt-123")
        .expect("the receipt links");
    let settled = store.read(&id).expect("card readable");
    assert_eq!(settled.receipt_id.as_deref(), Some("receipt-123"));
    assert_eq!(settled.audit_pending, None);
    assert_eq!(settled.audit_pending_reason, None);
}

// ---------------------------------------------------------------------------
// Cross-process coordination
// ---------------------------------------------------------------------------

/// Child-process mode for the cross-process proofs: re-executes this test
/// binary against an existing store. Exits 0 only when the other process's
/// transition is observed with the required semantics.
#[test]
fn cross_process_child() {
    let Some(dir) = std::env::var_os("ARDUR_APPROVALS_XPROC_DIR") else {
        return; // parent mode: not invoked as a child
    };
    let mode = std::env::var("ARDUR_APPROVALS_XPROC_MODE").expect("mode set");
    let id = std::env::var("ARDUR_APPROVALS_XPROC_ID").expect("id set");
    let store = ApprovalStore::new(std::path::PathBuf::from(dir).join("approvals"));
    match mode.as_str() {
        // The parent already claimed: our claim must observe AlreadySpent.
        "claim" => {
            let outcome = store
                .claim_execution(&id, &binding("shell.run", "deadbeef", Some("sess-1")), 50)
                .expect("claim observable across processes");
            assert!(
                matches!(outcome, ClaimOutcome::AlreadySpent(_)),
                "the child process must not win a second claim: {outcome:?}"
            );
        }
        // The parent already decided: our conflicting decision must conflict.
        "decide" => {
            let err = store
                .decide(
                    &id,
                    Decision::Reject {
                        reason: "child".to_string(),
                    },
                    50,
                )
                .expect_err("the child process must lose to the durable decision");
            assert!(matches!(err, ApprovalStoreError::AlreadyDecided));
        }
        other => panic!("unknown child mode {other}"),
    }
}

fn run_cross_process_child(dir: &std::path::Path, mode: &str, id: &str) -> std::process::Output {
    std::process::Command::new(std::env::current_exe().expect("test binary"))
        .arg("cross_process_child")
        .arg("--exact")
        .arg("--nocapture")
        .env("ARDUR_APPROVALS_XPROC_DIR", dir)
        .env("ARDUR_APPROVALS_XPROC_MODE", mode)
        .env("ARDUR_APPROVALS_XPROC_ID", id)
        .output()
        .expect("the child process spawns")
}

/// A claim won in THIS process is observed as spent by ANOTHER process — the
/// stable-coordination protocol is not per-process.
#[test]
fn a_claim_in_one_process_is_seen_as_spent_by_another() {
    let (dir, store) = store();
    let id = approved_card(&store);
    store
        .claim_execution(&id, &binding("shell.run", "deadbeef", Some("sess-1")), 42)
        .expect("the parent claim wins");

    let output = run_cross_process_child(dir.path(), "claim", &id);
    assert!(
        output.status.success(),
        "child must observe AlreadySpent: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A decision committed in THIS process conflicts a later decision from
/// ANOTHER process — cross-process single durable winner.
#[test]
fn a_decision_in_one_process_conflicts_a_decision_from_another() {
    let (dir, store) = store();
    let card = store
        .propose("shell.run", "shell.exec", "deadbeef", None, "r", 1)
        .expect("propose succeeds");
    let id = card.id.clone().unwrap();
    store
        .decide(&id, Decision::Approve, 10)
        .expect("the parent decision wins");

    let output = run_cross_process_child(dir.path(), "decide", &id);
    assert!(
        output.status.success(),
        "child must observe the explicit conflict: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        store.read(&id).expect("card readable").status,
        ApprovalStatus::Approved,
        "the durable record is untouched by the losing process"
    );
}
