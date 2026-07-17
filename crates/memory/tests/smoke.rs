//! smoke — the public Phase 1 surface exists, is name-stable, and is
//! object-safe. Exercises one write/read/invalidate cycle end to end.
use ardur_memory::{
    HolderId, InMemoryMemoryRuntime, InvalidationReason, MemoryRecord, MemoryRuntime, ReceiptId,
    RecordId, RecordKind, UnixTsMillis,
};
use serde_json::json;

#[test]
fn public_surface_is_usable() {
    let rt = InMemoryMemoryRuntime::new();
    let subject = HolderId::from("session:smoke");

    let rec = MemoryRecord::new(
        subject.clone(),
        RecordKind::Fact,
        json!({ "k": "v" }),
        UnixTsMillis(1),
        UnixTsMillis(1),
        None,
        UnixTsMillis(1),
    );
    let id: RecordId = rt.record(rec).unwrap();

    assert_eq!(rt.current_as_of(&subject, UnixTsMillis(2)).len(), 1);

    rt.invalidate(id, UnixTsMillis(3), InvalidationReason::Superseded)
        .unwrap();

    // The runtime is object-safe and the receipt-id type is part of the surface.
    let _dyn: &dyn MemoryRuntime = &rt;
    let _receipt: Option<ReceiptId> = None;
}
