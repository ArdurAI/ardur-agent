//! §1.7 integration tests: `FusedRuntime::compact` / `preview_compact` — a
//! session-domain context-compaction control that reuses the §1.8
//! checkpoint/receipt substrate.

mod support;

use std::sync::Arc;

use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, Role, SessionId};
use ardur_session_journals::{FileSessionJournal, JournalEntry, SessionJournal};

use support::{AUDIENCE, HOLDER, mint_token_as, permissive_policy};

fn compact_token() -> String {
    mint_token_as(HOLDER, AUDIENCE, &["context.compact"])
}

fn no_capability_token() -> String {
    mint_token_as(HOLDER, AUDIENCE, &["chat.submit"])
}

async fn build_runtime_with_journal(
    journal: Arc<FileSessionJournal>,
    provider: Arc<support::EchoProvider>,
) -> ardur_fused_runtime::FusedRuntime {
    support::runtime_builder_with_policy(provider, permissive_policy())
        .with_journal(journal)
        .build()
        .expect("runtime builds")
}

fn sample_history() -> Vec<ChatMessage> {
    vec![
        ChatMessage::user("please refactor the auth module"),
        ChatMessage::assistant("done, see auth.rs"),
        ChatMessage::user("now add tests"),
    ]
}

/// `compact` calls the provider (not the turn pipeline — no `UserMessage`/
/// `AssistantMessage` pollution), installs the result as a
/// `JournalEntry::Checkpoint`, and mints a chained receipt with the
/// `context.compact.applied.v1` verb.
#[tokio::test]
async fn compact_installs_a_checkpoint_without_polluting_the_turn_journal() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(dir.path(), session_id).expect("journal opens"));
    let provider = Arc::new(support::EchoProvider::new());
    let runtime = build_runtime_with_journal(journal.clone(), provider.clone()).await;
    let cap_token = CapTokenRef(compact_token());
    let history = sample_history();

    let outcome = runtime
        .compact(session_id, &cap_token, "context.compact", &history, None)
        .await
        .expect("compact succeeds");

    // The provider was actually called, over a request whose messages start
    // with the compaction system instruction, not a bare echo of `history`.
    assert_eq!(provider.call_count(), 1);
    let sent = provider.last_request().expect("a request was sent");
    assert_eq!(sent.messages[0].role, Role::System);
    assert!(sent.messages[0].content.contains("Active Task"));
    // EchoProvider echoes the last User message back as `content`.
    assert_eq!(outcome.summary, "now add tests");

    let entries = journal.replay(session_id).await.expect("journal replays");
    assert_eq!(
        entries.len(),
        1,
        "compact must not journal UserMessage/AssistantMessage entries for its own meta-call"
    );
    match &entries[0] {
        JournalEntry::Checkpoint {
            checkpoint_id,
            summary,
            ..
        } => {
            assert_eq!(*checkpoint_id, outcome.checkpoint_id);
            assert_eq!(summary, "now add tests");
        }
        other => panic!("expected a Checkpoint entry, got {other:?}"),
    }
}

/// A `focus` string is appended to the summarization instruction, steering
/// what the provider is asked to preserve.
#[tokio::test]
async fn compact_includes_the_focus_text_in_the_provider_request() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(dir.path(), session_id).expect("journal opens"));
    let provider = Arc::new(support::EchoProvider::new());
    let runtime = build_runtime_with_journal(journal, provider.clone()).await;
    let cap_token = CapTokenRef(compact_token());

    runtime
        .compact(
            session_id,
            &cap_token,
            "context.compact",
            &sample_history(),
            Some("the auth module changes".to_string()),
        )
        .await
        .expect("compact succeeds");

    let sent = provider.last_request().expect("a request was sent");
    assert!(sent.messages[0].content.contains("the auth module changes"));
}

/// `preview_compact` calls the provider and returns the summary, but
/// installs nothing: no journal entry, and (implicitly, since no journal
/// append happens) no receipt either.
#[tokio::test]
async fn preview_compact_does_not_install_anything() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(dir.path(), session_id).expect("journal opens"));
    let provider = Arc::new(support::EchoProvider::new());
    let runtime = build_runtime_with_journal(journal.clone(), provider.clone()).await;
    let cap_token = CapTokenRef(compact_token());

    let summary = runtime
        .preview_compact(&cap_token, "context.compact", &sample_history(), None)
        .await
        .expect("preview succeeds");

    assert_eq!(summary, "now add tests");
    assert_eq!(provider.call_count(), 1);
    let entries = journal.replay(session_id).await.expect("journal replays");
    assert!(entries.is_empty(), "preview must not append anything");
}

/// A compaction checkpoint restores through the exact same
/// `rollback_to_checkpoint` path a manual §1.8 checkpoint does.
#[tokio::test]
async fn a_compaction_checkpoint_is_restorable_via_rollback() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(dir.path(), session_id).expect("journal opens"));
    let provider = Arc::new(support::EchoProvider::new());
    let runtime = build_runtime_with_journal(journal, provider).await;
    let cap_token = CapTokenRef(compact_token());

    let compacted = runtime
        .compact(
            session_id,
            &cap_token,
            "context.compact",
            &sample_history(),
            None,
        )
        .await
        .expect("compact succeeds");

    let restored = runtime
        .rollback_to_checkpoint(
            session_id,
            &cap_token,
            "context.compact",
            compacted.checkpoint_id,
        )
        .await
        .expect("rollback to the compaction checkpoint succeeds");

    assert_eq!(restored.target_checkpoint_id, compacted.checkpoint_id);
    assert_eq!(restored.retained_entries.len(), 1);
}

/// A cap-token that does not grant `context.compact` is denied — and the
/// provider is never dispatched (verification happens before spending).
#[tokio::test]
async fn compact_is_denied_without_the_capability_and_never_spends() {
    let dir = tempfile::tempdir().expect("journal dir");
    let session_id = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(dir.path(), session_id).expect("journal opens"));
    let provider = Arc::new(support::EchoProvider::new());
    let runtime = build_runtime_with_journal(journal, provider.clone()).await;
    let cap_token = CapTokenRef(no_capability_token());

    let result = runtime
        .compact(
            session_id,
            &cap_token,
            "context.compact",
            &sample_history(),
            None,
        )
        .await;

    assert!(
        result.is_err(),
        "a token without context.compact must be denied"
    );
    assert_eq!(
        provider.call_count(),
        0,
        "the provider must not be dispatched when authorization fails"
    );
}

/// Two compactions chain their receipts together, and a subsequent turn's
/// receipt continues the *same* chain — the compaction receipt is not on a
/// side chain.
#[tokio::test]
async fn compaction_receipts_chain_with_turn_receipts() {
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
        &["context.compact", support::TOOL],
    ));

    let compacted = runtime
        .compact(
            session_id,
            &cap_token,
            "context.compact",
            &sample_history(),
            None,
        )
        .await
        .expect("compact succeeds");

    let turn = runtime
        .submit(support::request_for("hello", &cap_token.0, session_id))
        .await
        .expect("the turn completes");

    let chain = ardur_fused_runtime::load_persisted_chain(receipt_log.path()).expect("chain loads");
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].body.receipt_id, compacted.receipt_id.0);
    assert_eq!(
        chain[1].body.parent_hash,
        Some(ardur_receipt::Sha256Digest::of(
            chain[0].jws_compact.as_bytes()
        )),
        "the turn receipt must chain onto the compaction receipt"
    );
    assert_eq!(chain[1].body.receipt_id, turn.receipt_id.0);
    ardur_fused_runtime::verify_persisted_chain(&chain).expect("the chain verifies");
}
