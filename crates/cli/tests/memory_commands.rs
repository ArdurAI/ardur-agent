use std::sync::Arc;

use ardur_cli::FusedEngine;
use ardur_memory::{
    HolderId, InMemoryMemoryRuntime, MemoryRecord, MemoryRuntime, ReceiptId, RecordKind,
    UnixTsMillis,
};
use uuid::Uuid;

fn record(memory: &InMemoryMemoryRuntime, subject: &str, object: &str) -> Uuid {
    let receipt_id = Uuid::new_v4();
    let mut rec = MemoryRecord::new(
        HolderId::from(subject),
        RecordKind::Fact,
        serde_json::json!({
            "predicate": "remembers",
            "object": object,
            "source": "cli-test",
            "workspace_id": subject,
            "confidence": 0.77,
        }),
        UnixTsMillis(1_000),
        UnixTsMillis(1_000),
        None,
        UnixTsMillis(1_000),
    );
    rec.source_receipt_id = Some(ReceiptId(receipt_id));
    let id = rec.record_id;
    memory.record(rec).expect("record");
    id
}

#[test]
fn memory_list_show_forget_are_scoped_and_receipt_chained() {
    let memory = Arc::new(InMemoryMemoryRuntime::new());
    let mine = record(&memory, "workspace:mine", "blue deploy runbook");
    let other = record(&memory, "workspace:other", "other workspace secret");

    let list = FusedEngine::memory_command_on(&memory, "workspace:mine", "list", 2_000);
    assert!(
        list.contains(&mine.to_string()),
        "list includes same workspace id: {list}"
    );
    assert!(list.contains("cli-test"), "list shows provenance: {list}");
    assert!(
        list.contains("confidence=0.77"),
        "list shows confidence: {list}"
    );
    assert!(
        !list.contains(&other.to_string()),
        "list isolates other workspace: {list}"
    );

    let shown =
        FusedEngine::memory_command_on(&memory, "workspace:mine", &format!("show {mine}"), 2_000);
    assert!(
        shown.contains("blue deploy runbook"),
        "show returns card payload: {shown}"
    );
    assert!(
        shown.contains("receipt_id"),
        "show includes receipt provenance: {shown}"
    );

    let hidden =
        FusedEngine::memory_command_on(&memory, "workspace:mine", &format!("show {other}"), 2_000);
    assert!(
        hidden.contains("not found"),
        "show cannot cross workspace: {hidden}"
    );

    let forgot =
        FusedEngine::memory_command_on(&memory, "workspace:mine", &format!("forget {mine}"), 2_000);
    assert!(forgot.contains("forgot"), "forget succeeds: {forgot}");
    let after = FusedEngine::memory_command_on(&memory, "workspace:mine", "list", 3_000);
    assert!(
        !after.contains(&mine.to_string()),
        "forgotten memory is no longer current: {after}"
    );

    let history = memory.history_of(ardur_memory::RecordId(mine));
    assert!(
        history
            .iter()
            .any(|r| r.invalidation_time == Some(UnixTsMillis(2_000))
                && r.source_receipt_id.is_some()),
        "forget appends a receipt-chained tombstone"
    );
}

#[test]
fn memory_list_json_exports_cards() {
    let memory = Arc::new(InMemoryMemoryRuntime::new());
    let id = record(&memory, "workspace:json", "json export memory");

    let exported = FusedEngine::memory_command_on(&memory, "workspace:json", "list --json", 2_000);
    let cards: Vec<serde_json::Value> = serde_json::from_str(&exported).expect("valid JSON export");
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["record_id"], id.to_string());
    assert_eq!(cards[0]["source"], "cli-test");
    assert_eq!(cards[0]["scope"], "workspace:json");
}
