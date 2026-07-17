//! §7.10: appending N entries to an in-memory journal and replaying yields
//! exactly those N entries, in append order.

use ardur_session_journals::{InMemorySessionJournal, JournalEntry, SessionId, SessionJournal};

#[tokio::test]
async fn append_then_replay_preserves_count_and_order() {
    let session_id = SessionId::new();
    let journal = InMemorySessionJournal::new(session_id);

    for i in 0..5 {
        let id = journal
            .append(JournalEntry::UserMessage {
                content: format!("message {i}"),
                at: ardur_session_journals::UnixTsMillis(1_000 + i),
            })
            .await
            .expect("append");
        assert_eq!(id.value(), i, "entry ids are dense and monotonic");
    }

    let replayed = journal.replay(session_id).await.expect("replay");
    assert_eq!(replayed.len(), 5, "exactly the 5 appended entries");

    for (i, entry) in replayed.iter().enumerate() {
        match entry {
            JournalEntry::UserMessage { content, at } => {
                assert_eq!(content, &format!("message {i}"));
                assert_eq!(at.get(), 1_000 + i as u64);
            }
            other => panic!("unexpected entry at {i}: {other:?}"),
        }
    }
}

#[tokio::test]
async fn replay_with_foreign_session_id_is_not_found() {
    let journal = InMemorySessionJournal::new(SessionId::new());
    let err = journal
        .replay(SessionId::new())
        .await
        .expect_err("a foreign session id has no journal here");
    assert!(matches!(
        err,
        ardur_session_journals::JournalError::SessionNotFound(_)
    ));
}
