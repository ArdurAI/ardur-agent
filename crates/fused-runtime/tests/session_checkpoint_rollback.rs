//! §1.8 integration tests: `FusedRuntime::checkpoint` / `list_checkpoints` /
//! `rollback_to_checkpoint` — a session-domain checkpoint/rollback control
//! chained onto the same receipt chain turns use.

mod support;

use std::sync::Arc;

use ardur_runtime::{CapTokenRef, SessionId};
use ardur_session_journals::{FileSessionJournal, JournalEntry, SessionJournal};

use support::{AUDIENCE, HOLDER, cap_root, mint_token_as, permissive_policy, receipt_key};

/// A cap-token for [`HOLDER`] granting only the §1.8 session-control tools —
/// deliberately narrower than [`support::valid_token`] so a checkpoint/
/// rollback test never accidentally passes because the token also grants
/// `chat.submit`.
fn checkpoint_token() -> String {
    mint_token_as(
        HOLDER,
        AUDIENCE,
        &["session.checkpoint", "session.rollback"],
    )
}

fn checkpoint_only_token() -> String {
    mint_token_as(HOLDER, AUDIENCE, &["session.checkpoint"])
}

async fn build_runtime_with_journal(
    journal: Arc<FileSessionJournal>,
) -> ardur_fused_runtime::FusedRuntime {
    let provider = Arc::new(support::EchoProvider::new());
    support::runtime_builder_with_policy(provider, permissive_policy())
        .with_journal(journal)
        .build()
        .expect("runtime builds")
}

/// `checkpoint` records a `JournalEntry::Checkpoint`, mints a chained
/// receipt, and the returned summary defaults sensibly when no label is
/// given.
#[tokio::test]
async fn checkpoint_records_a_journal_entry_and_receipt() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(
        FileSessionJournal::new(dir.path(), session_id).expect("journal opens"),
    );
    let runtime = build_runtime_with_journal(journal.clone()).await;
    let cap_token = CapTokenRef(checkpoint_token());

    let outcome = runtime
        .checkpoint(session_id, &cap_token, "session.checkpoint", None)
        .await
        .expect("checkpoint succeeds");

    assert!(
        outcome.summary.contains("checkpoint"),
        "default summary should be descriptive: {}",
        outcome.summary
    );

    let entries = journal.replay(session_id).await.expect("journal replays");
    assert_eq!(entries.len(), 1);
    match &entries[0] {
        JournalEntry::Checkpoint { checkpoint_id, .. } => {
            assert_eq!(*checkpoint_id, outcome.checkpoint_id);
        }
        other => panic!("expected a Checkpoint entry, got {other:?}"),
    }
}

/// A caller-supplied label is used verbatim as the checkpoint summary.
#[tokio::test]
async fn checkpoint_uses_the_caller_supplied_label() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(
        FileSessionJournal::new(dir.path(), session_id).expect("journal opens"),
    );
    let runtime = build_runtime_with_journal(journal).await;
    let cap_token = CapTokenRef(checkpoint_token());

    let outcome = runtime
        .checkpoint(
            session_id,
            &cap_token,
            "session.checkpoint",
            Some("before the risky refactor".to_string()),
        )
        .await
        .expect("checkpoint succeeds");

    assert_eq!(outcome.summary, "before the risky refactor");
}

/// `list_checkpoints` returns every recorded checkpoint in creation order,
/// and does not mint a receipt for the (read-only) query.
#[tokio::test]
async fn list_checkpoints_returns_them_in_order() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(
        FileSessionJournal::new(dir.path(), session_id).expect("journal opens"),
    );
    let runtime = build_runtime_with_journal(journal).await;
    let cap_token = CapTokenRef(checkpoint_token());

    let first = runtime
        .checkpoint(
            session_id,
            &cap_token,
            "session.checkpoint",
            Some("first".to_string()),
        )
        .await
        .expect("first checkpoint");
    let second = runtime
        .checkpoint(
            session_id,
            &cap_token,
            "session.checkpoint",
            Some("second".to_string()),
        )
        .await
        .expect("second checkpoint");

    let checkpoints = runtime
        .list_checkpoints(session_id)
        .await
        .expect("list succeeds");
    assert_eq!(checkpoints.len(), 2);
    assert_eq!(checkpoints[0].checkpoint_id, first.checkpoint_id);
    assert_eq!(checkpoints[0].summary, "first");
    assert_eq!(checkpoints[1].checkpoint_id, second.checkpoint_id);
    assert_eq!(checkpoints[1].summary, "second");
}

/// `rollback_to_checkpoint` appends a `Rollback` marker naming the target
/// checkpoint, mints a chained receipt, and returns exactly the entries up
/// to and including that checkpoint — never deleting anything from the
/// underlying (still-replayable) journal.
#[tokio::test]
async fn rollback_appends_a_marker_and_returns_retained_entries() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(
        FileSessionJournal::new(dir.path(), session_id).expect("journal opens"),
    );
    let runtime = build_runtime_with_journal(journal.clone()).await;
    let cap_token = CapTokenRef(checkpoint_token());

    let checkpoint = runtime
        .checkpoint(session_id, &cap_token, "session.checkpoint", None)
        .await
        .expect("checkpoint succeeds");

    // Work that happens after the checkpoint — should be excluded from the
    // rollback's retained view, but must remain in the raw journal.
    journal
        .append(JournalEntry::Checkpoint {
            checkpoint_id: uuid::Uuid::new_v4(),
            summary: "later work".to_string(),
            at: 999,
        })
        .await
        .expect("later entry appends");

    let outcome = runtime
        .rollback_to_checkpoint(
            session_id,
            &cap_token,
            "session.rollback",
            checkpoint.checkpoint_id,
        )
        .await
        .expect("rollback succeeds");

    assert_eq!(outcome.target_checkpoint_id, checkpoint.checkpoint_id);
    // Retained view: just the original checkpoint entry.
    assert_eq!(outcome.retained_entries.len(), 1);

    // Nothing was deleted from the underlying journal: checkpoint + later
    // work + the rollback marker are all still there, in order.
    let raw = journal.replay(session_id).await.expect("journal replays");
    assert_eq!(raw.len(), 3);
    assert!(matches!(raw[0], JournalEntry::Checkpoint { .. }));
    assert!(matches!(raw[1], JournalEntry::Checkpoint { .. }));
    match &raw[2] {
        JournalEntry::Rollback {
            target_checkpoint_id,
            ..
        } => assert_eq!(*target_checkpoint_id, checkpoint.checkpoint_id),
        other => panic!("expected a Rollback entry, got {other:?}"),
    }
}

/// Rolling back to a checkpoint id that doesn't exist in this session's
/// journal is a clean typed error, not a panic — and mints no receipt or
/// journal entry.
#[tokio::test]
async fn rollback_to_unknown_checkpoint_is_rejected() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(
        FileSessionJournal::new(dir.path(), session_id).expect("journal opens"),
    );
    let runtime = build_runtime_with_journal(journal.clone()).await;
    let cap_token = CapTokenRef(checkpoint_token());

    let result = runtime
        .rollback_to_checkpoint(
            session_id,
            &cap_token,
            "session.rollback",
            uuid::Uuid::new_v4(),
        )
        .await;

    assert!(result.is_err(), "an unknown checkpoint id must be rejected");
    let entries = journal.replay(session_id).await.expect("journal replays");
    assert!(
        entries.is_empty(),
        "a rejected rollback must not append anything"
    );
}

/// A cap-token that does not grant `session.rollback` is denied at the
/// verification stage — checkpoint and rollback are distinct capabilities.
#[tokio::test]
async fn rollback_is_denied_without_the_rollback_capability() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(
        FileSessionJournal::new(dir.path(), session_id).expect("journal opens"),
    );
    let runtime = build_runtime_with_journal(journal.clone()).await;
    let full_cap_token = CapTokenRef(checkpoint_token());
    let checkpoint_only = CapTokenRef(checkpoint_only_token());

    let checkpoint = runtime
        .checkpoint(session_id, &full_cap_token, "session.checkpoint", None)
        .await
        .expect("checkpoint succeeds");

    let result = runtime
        .rollback_to_checkpoint(
            session_id,
            &checkpoint_only,
            "session.rollback",
            checkpoint.checkpoint_id,
        )
        .await;

    assert!(
        result.is_err(),
        "a token without session.rollback must be denied"
    );
}

/// Two checkpoint/rollback receipts chain onto the same receipt log a turn
/// would use — the control-plane receipt chain is not a parallel, weaker one.
#[tokio::test]
async fn checkpoint_and_rollback_receipts_chain_together() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(
        FileSessionJournal::new(dir.path(), session_id).expect("journal opens"),
    );
    let receipt_log = tempfile::NamedTempFile::new().expect("receipt log");
    let provider = Arc::new(support::EchoProvider::new());
    let runtime = support::runtime_builder_with_policy(provider, permissive_policy())
        .with_journal(journal)
        .receipt_log(receipt_log.path())
        .build()
        .expect("runtime builds");
    let cap_token = CapTokenRef(checkpoint_token());

    let checkpoint = runtime
        .checkpoint(session_id, &cap_token, "session.checkpoint", None)
        .await
        .expect("checkpoint succeeds");
    let rollback = runtime
        .rollback_to_checkpoint(
            session_id,
            &cap_token,
            "session.rollback",
            checkpoint.checkpoint_id,
        )
        .await
        .expect("rollback succeeds");

    let chain = ardur_fused_runtime::load_persisted_chain(receipt_log.path())
        .expect("chain loads");
    assert_eq!(chain.len(), 2);
    assert!(
        chain[0].body.parent_hash.is_none(),
        "first receipt is genesis"
    );
    assert_eq!(chain[0].body.receipt_id, checkpoint.receipt_id.0);
    assert_eq!(chain[1].body.receipt_id, rollback.receipt_id.0);
    ardur_fused_runtime::verify_persisted_chain(&chain).expect("the chain verifies");
    let _ = receipt_key();
    let _ = cap_root();
}
