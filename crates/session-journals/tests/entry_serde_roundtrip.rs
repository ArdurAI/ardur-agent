//! §7.10: every `JournalEntry` variant survives a JSON round-trip with all
//! fields intact — the on-disk JSONL format is lossless.

use ardur_session_journals::{
    CostDelta, CostTuple, JournalEntry, ReceiptId, ReservationId, Sha256Digest, ToolId,
};
use uuid::Uuid;

/// Serialize to JSON and back, asserting the value is unchanged.
fn assert_roundtrips(entry: &JournalEntry) {
    let json = serde_json::to_string(entry).expect("serialize");
    let back: JournalEntry = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(entry, &back, "round-trip changed the entry: {json}");
}

#[test]
fn every_variant_roundtrips() {
    let variants = vec![
        JournalEntry::UserMessage {
            content: "hello".into(),
            at: 1,
        },
        JournalEntry::AssistantMessage {
            content: "hi there".into(),
            at: 2,
            receipt_id: ReceiptId::new(),
        },
        JournalEntry::ToolInvocation {
            tool_id: ToolId::new("fs.read"),
            input_digest: Sha256Digest::of(b"the input payload"),
            output_digest: Sha256Digest::of(b"the output payload"),
            at: 3,
            receipt_id: ReceiptId::new(),
        },
        JournalEntry::CostFinalized {
            reservation_id: ReservationId::new(),
            actual: CostTuple {
                tokens_in: 100,
                tokens_out: 250,
                cents: 7,
                wall_ms: 1_500,
                attention_score: 3,
            },
            refunded: CostDelta {
                tokens_in: -10,
                tokens_out: 0,
                cents: 1,
                wall_ms: -50,
                attention_score: 0,
            },
            at: 4,
        },
        JournalEntry::Checkpoint {
            checkpoint_id: Uuid::new_v4(),
            summary: "state at turn 3".into(),
            at: 5,
        },
        JournalEntry::Invalidation {
            target_entry_id: ardur_session_journals::EntryId::new(2),
            reason: "superseded by a retry".into(),
            at: 6,
        },
    ];

    for entry in &variants {
        assert_roundtrips(entry);
    }

    // The discriminant is the tagged `kind` field.
    let json = serde_json::to_value(&variants[0]).expect("to_value");
    assert_eq!(json["kind"], "UserMessage");
}

#[test]
fn sha256_digest_validates_hex() {
    let good = Sha256Digest::of(b"x");
    assert_eq!(good.to_hex().len(), 64);
    assert!(Sha256Digest::from_hex(&good.to_hex()).is_ok());
    assert!(Sha256Digest::from_hex("too short").is_err());
    assert!(Sha256Digest::from_hex(&"Z".repeat(64)).is_err());
}
