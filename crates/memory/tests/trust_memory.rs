use ardur_memory::{
    HolderId, InMemoryMemoryRuntime, MemoryCard, MemoryRuntime, ReceiptId, RecordKind, UnixTsMillis,
};
use uuid::Uuid;

fn fact(
    subject: &str,
    object: &str,
    workspace: &str,
    receipt_id: Uuid,
) -> ardur_memory::MemoryRecord {
    let mut rec = ardur_memory::MemoryRecord::new(
        HolderId::from(subject),
        RecordKind::Fact,
        serde_json::json!({
            "predicate": "remembers",
            "object": object,
            "source": "session-journal",
            "workspace_id": workspace,
            "confidence": 0.82,
            "ttl_expires_at": 2_000_u64,
        }),
        UnixTsMillis(1_000),
        UnixTsMillis(1_000),
        None,
        UnixTsMillis(1_000),
    );
    rec.source_receipt_id = Some(ReceiptId(receipt_id));
    rec
}

#[test]
fn scoped_recall_returns_relevant_receipt_chained_memory_cards() {
    let memory = InMemoryMemoryRuntime::new();
    let subject = HolderId::from("workspace:a");
    let other = HolderId::from("workspace:b");
    let receipt_id = Uuid::new_v4();

    memory
        .record(fact(
            &subject.0,
            "deploy uses blue green strategy",
            "a",
            receipt_id,
        ))
        .expect("record relevant");
    memory
        .record(fact(&subject.0, "user likes espresso", "a", Uuid::new_v4()))
        .expect("record decoy");
    memory
        .record(fact(
            &other.0,
            "deploy uses canary strategy",
            "b",
            Uuid::new_v4(),
        ))
        .expect("record other workspace");

    let hits = memory
        .search_scoped(&subject, "blue green deploy", 5)
        .expect("scoped recall succeeds");

    assert_eq!(
        hits.len(),
        1,
        "only the matching same-workspace memory is recalled"
    );
    assert_eq!(hits[0].subject, subject);
    assert_eq!(hits[0].source_receipt_id, Some(ReceiptId(receipt_id)));

    let card = MemoryCard::from_record(&hits[0]);
    assert_eq!(card.source.as_deref(), Some("session-journal"));
    assert_eq!(card.scope.as_deref(), Some("a"));
    assert_eq!(card.confidence, Some(0.82));
    assert_eq!(card.ttl_expires_at, Some(UnixTsMillis(2_000)));
    assert_eq!(card.receipt_id, Some(ReceiptId(receipt_id)));
}

#[test]
fn empty_recall_query_returns_empty_without_scanning() {
    let memory = InMemoryMemoryRuntime::new();
    let subject = HolderId::from("workspace:empty");
    memory
        .record(fact(&subject.0, "anything", "empty", Uuid::new_v4()))
        .expect("record");

    assert!(memory.search("   ", 10).expect("search").is_empty());
    assert!(
        memory
            .search_scoped(&subject, "\n\t", 10)
            .expect("scoped search")
            .is_empty()
    );
}

#[test]
fn memory_card_export_preserves_provenance_and_validity() {
    let receipt_id = Uuid::new_v4();
    let rec = fact(
        "workspace:export",
        "retention window is 30 days",
        "export",
        receipt_id,
    );
    let card = MemoryCard::from_record(&rec);
    let exported = serde_json::to_value(&card).expect("card serializes");

    assert_eq!(exported["record_id"], rec.record_id.to_string());
    assert_eq!(exported["source"], "session-journal");
    assert_eq!(exported["scope"], "export");
    assert_eq!(exported["confidence"], 0.82);
    assert_eq!(exported["ttl_expires_at"], 2_000);
    assert_eq!(exported["receipt_id"], receipt_id.to_string());
    assert_eq!(exported["valid_from"], 1_000);
}
