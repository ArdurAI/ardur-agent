//! §7.10: a file-backed journal survives being dropped — reconstructing one
//! from the same path replays the entries written before the drop.

use std::io::Write as _;

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
                    at: ardur_session_journals::UnixTsMillis(42 + i),
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
                assert_eq!(at.get(), 42 + i as u64);
            }
            other => panic!("unexpected entry at {i}: {other:?}"),
        }
    }

    // A fresh handle continues the id sequence rather than restarting it.
    let next = reopened
        .append(JournalEntry::UserMessage {
            content: "after reopen".into(),
            at: ardur_session_journals::UnixTsMillis(99),
        })
        .await
        .expect("append after reopen");
    assert_eq!(next.value(), 3, "fourth entry lands at position 3");
}

#[tokio::test]
async fn file_journal_drops_and_truncates_torn_trailing_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_id = SessionId::new();

    let journal = FileSessionJournal::new(dir.path(), session_id).expect("open");
    journal
        .append(JournalEntry::UserMessage {
            content: "before crash".into(),
            at: ardur_session_journals::UnixTsMillis(1),
        })
        .await
        .expect("append first");
    let journal_path = journal.path().to_path_buf();
    journal.close().await.expect("close before torn write");

    std::fs::OpenOptions::new()
        .append(true)
        .open(&journal_path)
        .expect("open raw journal")
        .write_all(b"{\"type\":\"UserMessage\",\"content\":")
        .expect("write torn tail");

    let reopened = FileSessionJournal::new(dir.path(), session_id).expect("reopen repairs tail");
    assert_eq!(reopened.len().await.expect("len"), 1);
    let replayed = reopened.replay(session_id).await.expect("replay");
    assert_eq!(replayed.len(), 1);
    let next = reopened
        .append(JournalEntry::UserMessage {
            content: "after repair".into(),
            at: ardur_session_journals::UnixTsMillis(2),
        })
        .await
        .expect("append after repair");
    assert_eq!(
        next.value(),
        1,
        "new append follows the last complete entry"
    );

    let raw = std::fs::read_to_string(&journal_path).expect("read repaired file");
    assert_eq!(
        raw.lines().count(),
        2,
        "partial tail did not poison the next line"
    );
    for line in raw.lines() {
        serde_json::from_str::<JournalEntry>(line).expect("every remaining line is valid JSONL");
    }
}

#[cfg(unix)]
#[test]
fn file_journal_rejects_symlinked_file_and_session_directory() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("root");
    let outside = tempfile::tempdir().expect("outside");
    let session_id = SessionId::new();
    let session_dir = root.path().join("sessions").join(session_id.0.to_string());
    std::fs::create_dir_all(&session_dir).expect("session dir");
    let outside_file = outside.path().join("journal.jsonl");
    std::fs::write(&outside_file, "").expect("outside journal");
    symlink(&outside_file, session_dir.join("journal.jsonl")).expect("journal symlink");
    assert!(
        FileSessionJournal::new(root.path(), session_id).is_err(),
        "final journal symlink must fail closed"
    );

    std::fs::remove_dir_all(&session_dir).expect("remove session dir");
    let outside_session = outside.path().join("session");
    std::fs::create_dir_all(&outside_session).expect("outside session dir");
    symlink(&outside_session, &session_dir).expect("session directory symlink");
    assert!(
        FileSessionJournal::new(root.path(), session_id).is_err(),
        "session directory symlink must fail closed"
    );

    std::fs::remove_file(&session_dir).expect("remove session symlink");
    std::fs::remove_dir(root.path().join("sessions")).expect("remove sessions dir");
    let outside_sessions = outside.path().join("sessions");
    std::fs::create_dir_all(&outside_sessions).expect("outside sessions dir");
    symlink(&outside_sessions, root.path().join("sessions")).expect("sessions symlink");
    assert!(
        FileSessionJournal::new(root.path(), SessionId::new()).is_err(),
        "sessions parent symlink must fail closed"
    );

    let trusted = tempfile::tempdir().expect("trusted state root");
    let outside_base = outside.path().join("journals");
    std::fs::create_dir_all(&outside_base).expect("outside journal base");
    let symlinked_base = trusted.path().join("journals");
    symlink(&outside_base, &symlinked_base).expect("journal base symlink");
    assert!(
        FileSessionJournal::new(&symlinked_base, SessionId::new()).is_err(),
        "journal base symlink must fail closed"
    );
    assert!(
        std::fs::read_dir(&outside_base)
            .expect("outside base remains readable")
            .next()
            .is_none(),
        "journal creation must not escape through the symlinked base"
    );
}
