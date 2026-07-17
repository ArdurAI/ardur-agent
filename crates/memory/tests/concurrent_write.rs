//! §7.0 Phase 1 — concurrent appends are serialized and lossless.
//!
//! 10 threads each append 10 records (one record per (thread, subject)); the
//! store must end with exactly 100 records and no duplicate `record_id`s.
use std::collections::HashSet;
use std::sync::Arc;
use std::thread;

use ardur_memory::{HolderId, InMemoryMemoryRuntime, MemoryRuntime, RecordKind, UnixTsMillis};
use serde_json::json;

#[test]
fn ten_threads_ten_records_each() {
    let rt = Arc::new(InMemoryMemoryRuntime::new());

    let handles: Vec<_> = (0..10)
        .map(|t| {
            let rt = Arc::clone(&rt);
            thread::spawn(move || {
                for i in 0..10 {
                    let rec = ardur_memory::MemoryRecord::new(
                        HolderId::from(format!("user:{t}")),
                        RecordKind::Observation,
                        json!({ "t": t, "i": i }),
                        UnixTsMillis(0),
                        UnixTsMillis(0),
                        None,
                        UnixTsMillis(0),
                    );
                    rt.record(rec).unwrap();
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    // Count across all subjects and collect ids to check for loss/duplication.
    let mut ids = HashSet::new();
    let mut total = 0;
    for t in 0..10 {
        let recs = rt.current_as_of(&HolderId::from(format!("user:{t}")), UnixTsMillis(1));
        total += recs.len();
        for r in recs {
            assert!(ids.insert(r.record_id), "duplicate record_id observed");
        }
    }

    assert_eq!(total, 100);
    assert_eq!(ids.len(), 100);
}
