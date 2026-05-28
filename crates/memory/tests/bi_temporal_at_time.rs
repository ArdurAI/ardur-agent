//! §7.0 Phase 1 — overlapping valid-time intervals are resolved by `at_time`.
//!
//! A valid 100..200, B valid 150..250:
//!   at_time(120) -> [A]; (175) -> [A, B]; (225) -> [B]; (300) -> [].
use ardur_memory::{
    HolderId, InMemoryMemoryRuntime, MemoryRecord, MemoryRuntime, RecordKind, UnixTsMillis,
};
use serde_json::{Value, json};

fn payloads(mut recs: Vec<MemoryRecord>) -> Vec<Value> {
    recs.sort_by(|a, b| a.valid_from.cmp(&b.valid_from));
    recs.into_iter().map(|r| r.payload).collect()
}

#[test]
fn overlapping_valid_intervals() {
    let rt = InMemoryMemoryRuntime::new();
    let s = HolderId::from("s");

    let a = MemoryRecord::new(
        s.clone(),
        RecordKind::Fact,
        json!("A"),
        UnixTsMillis(100),
        UnixTsMillis(100),
        Some(UnixTsMillis(200)),
        UnixTsMillis(100),
    );
    let b = MemoryRecord::new(
        s.clone(),
        RecordKind::Fact,
        json!("B"),
        UnixTsMillis(150),
        UnixTsMillis(150),
        Some(UnixTsMillis(250)),
        UnixTsMillis(150),
    );
    rt.record(a).unwrap();
    rt.record(b).unwrap();

    assert_eq!(
        payloads(rt.at_time(&s, UnixTsMillis(120))),
        vec![json!("A")]
    );
    assert_eq!(
        payloads(rt.at_time(&s, UnixTsMillis(175))),
        vec![json!("A"), json!("B")]
    );
    assert_eq!(
        payloads(rt.at_time(&s, UnixTsMillis(225))),
        vec![json!("B")]
    );
    assert!(rt.at_time(&s, UnixTsMillis(300)).is_empty());
}
