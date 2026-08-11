//! §7.10 regression: the registry must release a session's journal — and its
//! backing file descriptor — when the session ends, rather than leaking one per
//! session for the life of the process (issue #351).
//!
//! Before the fix, [`JournalRegistry`] `Box::leak`ed every journal into a
//! `'static` reference: the journal's `Drop` never ran and its FD never closed,
//! so a long-running server accumulated one leaked journal + one FD per session
//! ever seen. These tests fail under that behavior and pass with `Arc`-handle
//! storage plus [`JournalRegistry::remove`] eviction.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use ardur_session_journals::{
    EntryId, FileSessionJournal, InMemorySessionJournal, JournalEntry, JournalError,
    JournalRegistry, SessionId, SessionJournal, UnixTsMillis,
};

/// A journal that increments a shared counter when it is dropped, so a test can
/// prove the registry actually destructs it rather than leaking it. Everything
/// else delegates to an inner in-memory journal.
struct DropCounting {
    inner: InMemorySessionJournal,
    dropped: Arc<AtomicUsize>,
}

impl Drop for DropCounting {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl SessionJournal for DropCounting {
    async fn append(&self, entry: JournalEntry) -> Result<EntryId, JournalError> {
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

/// Running many sessions through the registry — each registered, used, then
/// removed on session end — must destruct every journal, leaving none alive.
#[tokio::test]
async fn removed_journals_are_destructed_not_leaked() {
    let registry = JournalRegistry::new();
    let dropped = Arc::new(AtomicUsize::new(0));

    const SESSIONS: usize = 500;
    for _ in 0..SESSIONS {
        let session_id = SessionId::new();
        let journal: Arc<dyn SessionJournal> = Arc::new(DropCounting {
            inner: InMemorySessionJournal::new(session_id),
            dropped: Arc::clone(&dropped),
        });
        registry.register(journal).expect("register");

        // A live handle appends through the session...
        let handle = registry.get(&session_id).expect("live handle");
        handle
            .append(JournalEntry::UserMessage {
                content: "turn".into(),
                at: UnixTsMillis(1),
            })
            .await
            .expect("append");
        drop(handle);

        // ...then the session ends: evict, flush/close, release the handle.
        let evicted = registry.remove(&session_id).expect("remove returns handle");
        evicted.close().await.expect("close");
        drop(evicted);

        assert!(!registry.contains(&session_id));
    }

    assert!(registry.is_empty(), "registry retains no finished sessions");
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        SESSIONS,
        "every removed journal is destructed; under the old Box::leak this was 0"
    );
}

/// Count the process's currently-open file descriptors via `/dev/fd`, which is
/// present on both macOS and Linux.
#[cfg(unix)]
fn open_fd_count() -> usize {
    std::fs::read_dir("/dev/fd").expect("read /dev/fd").count()
}

/// A file-backed journal opens an FD. Running many file-backed sessions through
/// the registry, removing each on session end, must not grow the process's FD
/// count — the leak this guards against was unbounded FD growth ending in FD
/// exhaustion on the long-lived `ardur-server`.
#[cfg(unix)]
#[tokio::test]
async fn removed_file_journals_release_their_descriptors() {
    let base = tempfile::tempdir().expect("tempdir");
    let registry = JournalRegistry::new();

    // Warm up once so any one-time FD allocation (dir handles, tracing, etc.)
    // is not attributed to the measured loop.
    {
        let sid = SessionId::new();
        let j: Arc<dyn SessionJournal> =
            Arc::new(FileSessionJournal::new(base.path(), sid).expect("open journal"));
        registry.register(j).expect("register");
        let evicted = registry.remove(&sid).expect("remove");
        evicted.close().await.expect("close");
        drop(evicted);
    }

    let before = open_fd_count();

    const SESSIONS: usize = 400;
    for _ in 0..SESSIONS {
        let session_id = SessionId::new();
        let journal: Arc<dyn SessionJournal> =
            Arc::new(FileSessionJournal::new(base.path(), session_id).expect("open journal"));
        registry.register(journal).expect("register");

        let handle = registry.get(&session_id).expect("live handle");
        handle
            .append(JournalEntry::UserMessage {
                content: "turn".into(),
                at: UnixTsMillis(1),
            })
            .await
            .expect("append");
        drop(handle);

        // Session end: evict, close, release — the FD must go with it.
        let evicted = registry.remove(&session_id).expect("remove");
        evicted.close().await.expect("close");
        drop(evicted);
    }

    let after = open_fd_count();
    assert!(registry.is_empty(), "no finished session is retained");
    // With the old Box::leak, each iteration would leak one FD and `after`
    // would exceed `before` by ~SESSIONS. A small slack absorbs unrelated
    // descriptors the async runtime may open concurrently during the loop.
    assert!(
        after <= before + 8,
        "file descriptors leaked across sessions: before={before}, after={after}"
    );
}
