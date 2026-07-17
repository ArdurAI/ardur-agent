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

/// ARD-477: recall (`search` / `search_scoped`) must not surface a memory whose
/// correction chain has been invalidated. Invalidation is append-only — the
/// original row keeps `invalidation_time = None` and a tombstone is appended —
/// so the only thing that can hide the original from recall is the chain-level
/// cutoff, which is exactly what the fix adds.
#[test]
fn search_honors_invalidation() {
    let rt = InMemoryMemoryRuntime::new();
    let s = HolderId::from("s");

    let a = MemoryRecord::new(
        s.clone(),
        RecordKind::Fact,
        json!({ "predicate": "prefers", "object": "oolong tea" }),
        UnixTsMillis(100),
        UnixTsMillis(100),
        None,
        UnixTsMillis(100),
    );
    let a_id = rt.record(a).unwrap();

    // Before invalidation the memory is recalled (payload text lowercases to
    // "prefers oolong tea", so the "oolong" query matches).
    assert_eq!(
        rt.search("oolong", 5).expect("search").len(),
        1,
        "a live memory is recalled before invalidation"
    );
    assert_eq!(
        rt.search_scoped(&s, "oolong", 5)
            .expect("scoped search")
            .len(),
        1,
    );

    rt.invalidate(a_id, UnixTsMillis(200), InvalidationReason::UserCorrection)
        .unwrap();

    // After invalidation the original row still has `invalidation_time = None`
    // (the tombstone is a separate appended row), so without the chain-level
    // cutoff it would still be recalled. The fix excludes the whole chain.
    assert!(
        rt.search("oolong", 5).expect("search").is_empty(),
        "a forgotten memory is not re-injected by recall"
    );
    assert!(
        rt.search_scoped(&s, "oolong", 5)
            .expect("scoped search")
            .is_empty(),
        "a forgotten memory is not re-injected by scoped recall"
    );
}

fn uuid_v4() -> uuid::Uuid {
    uuid::Uuid::new_v4()
}
