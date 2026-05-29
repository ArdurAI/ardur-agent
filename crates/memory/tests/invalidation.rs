//! §7.0 Phase 1 — invalidation is append-only and chain-aware.
//!
//! Record A valid 100..forever, then invalidate(A, at=200, UserCorrection):
//!   at_time(150) -> [A] (before the cutoff)
//!   at_time(250) -> []  (after the cutoff)
//!   history_of(A) -> [A, invalidation row]  (the original is never mutated)
use ardur_memory::{
    HolderId, InMemoryMemoryRuntime, InvalidationReason, MemoryRecord, MemoryRuntime, RecordKind,
    UnixTsMillis,
};
use serde_json::json;

#[test]
fn invalidate_appends_and_cuts_the_chain() {
    let rt = InMemoryMemoryRuntime::new();
    let s = HolderId::from("s");

    let a = MemoryRecord::new(
        s.clone(),
        RecordKind::Fact,
        json!("A"),
        UnixTsMillis(100),
        UnixTsMillis(100),
        None,
        UnixTsMillis(100),
    );
    let a_id = rt.record(a).unwrap();

    // Live before invalidation.
    assert_eq!(rt.at_time(&s, UnixTsMillis(150)).len(), 1);

    rt.invalidate(a_id, UnixTsMillis(200), InvalidationReason::UserCorrection)
        .unwrap();

    // Still visible before the cutoff; gone at/after it.
    assert_eq!(rt.at_time(&s, UnixTsMillis(150)).len(), 1);
    assert!(rt.at_time(&s, UnixTsMillis(250)).is_empty());

    // The original row plus the appended invalidation row are both retained.
    let history = rt.history_of(a_id);
    assert_eq!(history.len(), 2);
    // Exactly one is the original (no invalidation_time), one is the tombstone.
    assert_eq!(
        history
            .iter()
            .filter(|r| r.invalidation_time.is_none())
            .count(),
        1
    );
    assert_eq!(
        history
            .iter()
            .filter(|r| r.invalidation_time == Some(UnixTsMillis(200)))
            .count(),
        1
    );

    // Unknown id -> NotFound.
    use ardur_memory::{MemoryError, RecordId};
    let bogus = RecordId(uuid_v4());
    match rt.invalidate(bogus, UnixTsMillis(1), InvalidationReason::Expired) {
        Err(MemoryError::NotFound(_)) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
}

fn uuid_v4() -> uuid::Uuid {
    uuid::Uuid::new_v4()
}
