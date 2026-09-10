//! §7.10: concurrent appends are serialized — 10 threads each appending 10
//! entries yields exactly 100 entries with monotonic, gap-free, duplicate-free
//! `EntryId`s.

use ardur_session_journals::{InMemorySessionJournal, JournalEntry, SessionId, SessionJournal};
use std::sync::Arc;

const THREADS: u64 = 10;
const PER_THREAD: u64 = 10;

#[test]
fn concurrent_appends_yield_unique_monotonic_ids() {
    let session_id = SessionId::new();
    let journal = Arc::new(InMemorySessionJournal::new(session_id));

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let journal = Arc::clone(&journal);
            std::thread::spawn(move || {
                // Each worker drives its own current-thread runtime to poll the
                // async appends; the journal's lock serializes them.
                let rt = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .expect("runtime");
                rt.block_on(async {
                    let mut ids = Vec::with_capacity(PER_THREAD as usize);
                    for i in 0..PER_THREAD {
                        let id = journal
                            .append(JournalEntry::UserMessage {
                                content: format!("t{t}-{i}"),
                                at: ardur_session_journals::UnixTsMillis(t * 1_000 + i),
                            })
                            .await
                            .expect("append");
                        ids.push(id.value());
                    }
                    ids
                })
            })
        })
        .collect();

    let mut all_ids: Vec<u64> = handles
        .into_iter()
        .flat_map(|h| h.join().expect("thread joined"))
        .collect();

    let total = (THREADS * PER_THREAD) as usize;
    assert_eq!(all_ids.len(), total, "every append returned an id");
    assert_eq!(journal.len(), total, "every entry landed in the log");

    all_ids.sort_unstable();
    all_ids.dedup();
    assert_eq!(all_ids.len(), total, "no duplicate ids were handed out");
    assert_eq!(
        all_ids,
        (0..total as u64).collect::<Vec<_>>(),
        "ids are dense and monotonic from 0"
    );
}
