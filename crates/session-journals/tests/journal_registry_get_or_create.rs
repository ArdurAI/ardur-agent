//! §7.10: `get_or_create` resolves to the *same* journal on a second call for
//! the same session — the factory runs once, and the second caller observes
//! state written through the first handle.

use ardur_session_journals::{
    InMemorySessionJournal, JournalEntry, JournalRegistry, SessionId, SessionJournal,
};
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn get_or_create_returns_the_same_journal() {
    let registry = JournalRegistry::new();
    let session_id = SessionId::new();
    let factory_calls = AtomicUsize::new(0);

    let factory = || {
        factory_calls.fetch_add(1, Ordering::SeqCst);
        Box::new(InMemorySessionJournal::new(session_id)) as Box<dyn SessionJournal>
    };

    // First call mints the journal and appends through it.
    let first = registry
        .get_or_create(&session_id, factory)
        .expect("get_or_create");
    first
        .append(JournalEntry::UserMessage {
            content: "via first handle".into(),
            at: ardur_session_journals::UnixTsMillis(7),
        })
        .await
        .expect("append");

    // Second call must return the same journal — not a fresh empty one.
    let second = registry
        .get_or_create(&session_id, factory)
        .expect("get_or_create");

    assert_eq!(
        factory_calls.load(Ordering::SeqCst),
        1,
        "the factory runs only on the first call"
    );

    let replayed = second.replay(session_id).await.expect("replay");
    assert_eq!(
        replayed.len(),
        1,
        "the second handle sees what was written through the first"
    );
    assert!(registry.contains(&session_id));
    assert_eq!(registry.len(), 1);
}

#[tokio::test]
async fn register_rejects_a_duplicate_session() {
    let registry = JournalRegistry::new();
    let session_id = SessionId::new();

    registry
        .register(Box::new(InMemorySessionJournal::new(session_id)))
        .expect("first registration");
    let err = registry
        .register(Box::new(InMemorySessionJournal::new(session_id)))
        .expect_err("duplicate registration");
    assert!(matches!(
        err,
        ardur_session_journals::RegistryError::AlreadyRegistered(_)
    ));
}
