//! §7.10: `replay_from` resumes after a checkpoint — given the checkpoint's
//! `EntryId`, it returns only the entries recorded after it.

use ardur_session_journals::{
    EntryId, InMemorySessionJournal, JournalEntry, SessionId, SessionJournal,
};
use uuid::Uuid;

#[tokio::test]
async fn replay_from_a_checkpoint_returns_only_later_entries() {
    let session_id = SessionId::new();
    let journal = InMemorySessionJournal::new(session_id);

    let mut checkpoint_id: Option<EntryId> = None;

    // Append 10 entries; the 5th (0-based index 4) is a Checkpoint.
    for i in 0..10u64 {
        let entry = if i == 4 {
            JournalEntry::Checkpoint {
                checkpoint_id: Uuid::new_v4(),
                summary: "halfway".into(),
                at: ardur_session_journals::UnixTsMillis(5_000 + i),
            }
        } else {
            JournalEntry::UserMessage {
                content: format!("message {i}"),
                at: ardur_session_journals::UnixTsMillis(5_000 + i),
            }
        };
        let id = journal.append(entry).await.expect("append");
        if i == 4 {
            checkpoint_id = Some(id);
        }
    }

    let checkpoint_id = checkpoint_id.expect("checkpoint recorded");

    let resumed = journal
        .replay_from(session_id, checkpoint_id)
        .await
        .expect("replay_from");

    // Entries after the 5th: indices 5,6,7,8,9 — five entries, none a checkpoint.
    assert_eq!(
        resumed.len(),
        5,
        "the entries recorded after the checkpoint"
    );
    for (offset, entry) in resumed.iter().enumerate() {
        let i = 5 + offset as u64;
        match entry {
            JournalEntry::UserMessage { content, at } => {
                assert_eq!(content, &format!("message {i}"));
                assert_eq!(at.get(), 5_000 + i);
            }
            other => panic!("unexpected entry after checkpoint: {other:?}"),
        }
    }
}

#[tokio::test]
async fn replay_from_past_the_end_is_not_found() {
    let session_id = SessionId::new();
    let journal = InMemorySessionJournal::new(session_id);
    journal
        .append(JournalEntry::UserMessage {
            content: "only one".into(),
            at: ardur_session_journals::UnixTsMillis(1),
        })
        .await
        .expect("append");

    let err = journal
        .replay_from(session_id, EntryId::new(5))
        .await
        .expect_err("cursor past the end");
    assert!(matches!(
        err,
        ardur_session_journals::JournalError::EntryNotFound(_)
    ));
}
