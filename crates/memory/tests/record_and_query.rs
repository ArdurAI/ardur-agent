//! §7.0 Phase 1 — record three facts for a subject, then read them back via the
//! current-as-of view.
use ardur_memory::{
    HolderId, InMemoryMemoryRuntime, MemoryRecord, MemoryRuntime, RecordKind, UnixTsMillis,
};
use serde_json::json;

#[test]
fn record_three_facts_and_query_current() {
    let rt = InMemoryMemoryRuntime::new();
    let subject = HolderId::from("user:alice");
    let now = UnixTsMillis(1_000);

    for i in 0..3 {
        let rec = MemoryRecord::new(
            subject.clone(),
            RecordKind::Fact,
            json!({ "i": i }),
            now,
            UnixTsMillis(0),
            None,
            now,
        );
        rt.record(rec).unwrap();
    }

    let current = rt.current_as_of(&subject, now);
    assert_eq!(current.len(), 3);

    // A different subject sees nothing.
    assert!(
        rt.current_as_of(&HolderId::from("user:bob"), now)
            .is_empty()
    );
}
