//! §7.10: a file-backed journal survives being dropped — reconstructing one
//! from the same path replays the entries written before the drop.

use ardur_session_journals::{FileSessionJournal, JournalEntry, SessionId, SessionJournal};

#[tokio::test]
async fn file_journal_roundtrips_across_a_drop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = SessionId::new();

    // Write three entries, then drop the journal entirely.
    {
        let journal = FileSessionJournal::new(dir.path(), session_id).expect("open");
        for i in 0..3 {
            journal
                .append(JournalEntry::UserMessage {
                    content: format!("persisted {i}"),
                    at: 42 + i,
                })
                .await
                .expect("append");
        }
        journal.close().await.expect("close");
    }

    // Reconstruct from the same path and replay.
    let reopened = FileSessionJournal::new(dir.path(), session_id).expect("reopen");
    let replayed = reopened.replay(session_id).await.expect("replay");

    assert_eq!(replayed.len(), 3, "the 3 entries persisted before the drop");
    for (i, entry) in replayed.iter().enumerate() {
        match entry {
            JournalEntry::UserMessage { content, at } => {
                assert_eq!(content, &format!("persisted {i}"));
                assert_eq!(*at, 42 + i as u64);
            }
            other => panic!("unexpected entry at {i}: {other:?}"),
        }
    }

    // A fresh handle continues the id sequence rather than restarting it.
    let next = reopened
        .append(JournalEntry::UserMessage {
            content: "after reopen".into(),
            at: 99,
        })
        .await
        .expect("append after reopen");
    assert_eq!(next.value(), 3, "fourth entry lands at position 3");
}
