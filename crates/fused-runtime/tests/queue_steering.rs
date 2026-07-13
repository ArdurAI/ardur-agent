//! §1.10 integration tests: `FusedRuntime::accept_steer_directive` /
//! `accept_interrupt` — receipted, chained evidence that a steering or
//! interrupt request against a target background task was accepted, per
//! the documented KNOWN LIMITATION that neither yet changes the target
//! task's in-flight behavior (that requires an iterative task-flow runtime,
//! deferred beyond this MVP's one-shot background task).

mod support;

use std::sync::Arc;

use ardur_runtime::{CapTokenRef, SessionId};
use ardur_session_journals::{FileSessionJournal, SessionJournal};

use support::{AUDIENCE, HOLDER, mint_token_as, permissive_policy};

fn steer_token() -> String {
    mint_token_as(HOLDER, AUDIENCE, &["input.steer"])
}

fn interrupt_token() -> String {
    mint_token_as(HOLDER, AUDIENCE, &["input.interrupt"])
}

fn no_capability_token() -> String {
    mint_token_as(HOLDER, AUDIENCE, &["chat.submit"])
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

/// A steering directive mints a real, fresh `input.steer.accepted.v1`
/// receipt and does not touch the foreground session journal — it is a
/// control-plane action against a background task, not a chat turn.
#[tokio::test]
async fn accept_steer_directive_mints_a_receipt_without_journaling() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(dir.path(), session_id).expect("journal opens"));
    let runtime = build_runtime_with_journal(journal.clone()).await;
    let cap_token = CapTokenRef(steer_token());
    let target_task_id = uuid::Uuid::now_v7();

    let receipt_id = runtime
        .accept_steer_directive(
            session_id,
            &cap_token,
            "input.steer",
            target_task_id,
            "focus on the auth module first",
        )
        .await
        .expect("the steer call succeeds");

    assert_ne!(receipt_id.0, uuid::Uuid::nil());
    let entries = journal.replay(session_id).await.expect("journal replays");
    assert!(
        entries.is_empty(),
        "accepting a steer directive must not journal into the foreground session"
    );
}

/// An interrupt mints a real, fresh `input.interrupt.accepted.v1` receipt.
#[tokio::test]
async fn accept_interrupt_mints_a_receipt() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(dir.path(), session_id).expect("journal opens"));
    let runtime = build_runtime_with_journal(journal).await;
    let cap_token = CapTokenRef(interrupt_token());
    let target_task_id = uuid::Uuid::now_v7();

    let receipt_id = runtime
        .accept_interrupt(session_id, &cap_token, "input.interrupt", target_task_id)
        .await
        .expect("the interrupt call succeeds");

    assert_ne!(receipt_id.0, uuid::Uuid::nil());
}

/// A steering directive is denied without the `input.steer` capability.
#[tokio::test]
async fn accept_steer_directive_is_denied_without_the_capability() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(dir.path(), session_id).expect("journal opens"));
    let runtime = build_runtime_with_journal(journal).await;
    let cap_token = CapTokenRef(no_capability_token());

    let result = runtime
        .accept_steer_directive(
            session_id,
            &cap_token,
            "input.steer",
            uuid::Uuid::now_v7(),
            "hello",
        )
        .await;

    assert!(result.is_err());
}

/// An interrupt is denied without the `input.interrupt` capability.
#[tokio::test]
async fn accept_interrupt_is_denied_without_the_capability() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(dir.path(), session_id).expect("journal opens"));
    let runtime = build_runtime_with_journal(journal).await;
    let cap_token = CapTokenRef(no_capability_token());

    let result = runtime
        .accept_interrupt(session_id, &cap_token, "input.interrupt", uuid::Uuid::now_v7())
        .await;

    assert!(result.is_err());
}

/// Steer and interrupt receipts share the same chain as an ordinary turn —
/// §1.10's control actions are not on a side chain.
#[tokio::test]
async fn steer_and_interrupt_receipts_chain_with_turn_receipts() {
    use ardur_runtime::ChatRuntime;

    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(dir.path(), session_id).expect("journal opens"));
    let receipt_log = tempfile::NamedTempFile::new().expect("receipt log");
    let provider = Arc::new(support::EchoProvider::new());
    let runtime = support::runtime_builder_with_policy(provider, permissive_policy())
        .with_journal(journal)
        .receipt_log(receipt_log.path())
        .build()
        .expect("runtime builds");
    let cap_token = CapTokenRef(mint_token_as(
        HOLDER,
        AUDIENCE,
        &["input.steer", "input.interrupt", support::TOOL],
    ));
    let target_task_id = uuid::Uuid::now_v7();

    let steer = runtime
        .accept_steer_directive(
            session_id,
            &cap_token,
            "input.steer",
            target_task_id,
            "narrow the scope",
        )
        .await
        .expect("steer accepted");
    let interrupt = runtime
        .accept_interrupt(session_id, &cap_token, "input.interrupt", target_task_id)
        .await
        .expect("interrupt accepted");
    let turn = runtime
        .submit(support::request_for("hello", &cap_token.0, session_id))
        .await
        .expect("the turn completes");

    let chain =
        ardur_fused_runtime::load_persisted_chain(receipt_log.path()).expect("chain loads");
    assert_eq!(chain.len(), 3);
    assert_eq!(chain[0].body.receipt_id, steer.0);
    assert_eq!(chain[1].body.receipt_id, interrupt.0);
    assert_eq!(chain[2].body.receipt_id, turn.receipt_id.0);
    ardur_fused_runtime::verify_persisted_chain(&chain).expect("the chain verifies");
}
