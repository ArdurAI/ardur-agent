//! §7.0 Phase 1 — every `RecordKind` survives a serde round-trip, both on its
//! own and embedded in a full `MemoryRecord`.
use ardur_memory::{HolderId, MemoryRecord, RecordKind, UnixTsMillis};
use serde_json::json;

const ALL_KINDS: [RecordKind; 6] = [
    RecordKind::Fact,
    RecordKind::Observation,
    RecordKind::Preference,
    RecordKind::ContextSnapshot,
    RecordKind::Decision,
    RecordKind::Reflection,
];

#[test]
fn record_kind_roundtrips() {
    for kind in ALL_KINDS {
        let encoded = serde_json::to_string(&kind).unwrap();
        let decoded: RecordKind = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, kind, "kind tag not preserved: {encoded}");
    }
}

#[test]
fn memory_record_roundtrips_each_kind() {
    for kind in ALL_KINDS {
        let rec = MemoryRecord::new(
            HolderId::from("s"),
            kind,
            json!({ "note": "payload" }),
            UnixTsMillis(10),
            UnixTsMillis(10),
            None,
            UnixTsMillis(10),
        );
        let encoded = serde_json::to_string(&rec).unwrap();
        let decoded: MemoryRecord = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, rec);
        assert_eq!(decoded.kind, kind);
    }
}
