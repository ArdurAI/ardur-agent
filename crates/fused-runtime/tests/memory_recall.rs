use std::sync::Arc;

use ardur_memory::{
    HolderId, InMemoryMemoryRuntime, MemoryRecord, MemoryRuntime, ReceiptId, RecordKind,
    UnixTsMillis,
};
use ardur_runtime::ChatRuntime;

mod support;
use support::{EchoProvider, HOLDER, request_for, runtime_builder, valid_token};

fn memory_fact(subject: &str, object: &str, receipt_id: uuid::Uuid) -> MemoryRecord {
    memory_fact_with_confidence(subject, object, 0.9, receipt_id)
}

fn memory_fact_with_confidence(
    subject: &str,
    object: &str,
    confidence: f64,
    receipt_id: uuid::Uuid,
) -> MemoryRecord {
    let mut rec = MemoryRecord::new(
        HolderId::from(subject),
        RecordKind::Fact,
        serde_json::json!({
            "predicate": "deployment fact",
            "object": object,
            "source": "session-journal",
            "workspace_id": subject,
            "confidence": confidence,
        }),
        UnixTsMillis(1_750_000_000_000),
        UnixTsMillis(1_750_000_000_000),
        None,
        UnixTsMillis(1_750_000_000_000),
    );
    rec.source_receipt_id = Some(ReceiptId(receipt_id));
    rec
}

#[tokio::test]
async fn turn_path_injects_scoped_recalled_memories_after_cap_and_cedar() {
    let provider = Arc::new(EchoProvider::new());
    let memory = Arc::new(InMemoryMemoryRuntime::new());
    let expected_receipt = uuid::Uuid::new_v4();

    memory
        .record(memory_fact(
            HOLDER,
            "blue green deploy requires warming the standby pool",
            expected_receipt,
        ))
        .expect("record relevant memory");
    memory
        .record(memory_fact(
            "spiffe://ardur/user/other-workspace",
            "blue green deploy in a different workspace must not leak",
            uuid::Uuid::new_v4(),
        ))
        .expect("record isolated memory");

    let runtime = runtime_builder(provider.clone())
        .with_memory(memory)
        .build()
        .expect("runtime builds");

    runtime
        .submit(request_for(
            "How should we do the blue green deploy?",
            &valid_token(),
            ardur_runtime::SessionId::new(),
        ))
        .await
        .expect("turn succeeds");

    let request = provider.last_request().expect("provider saw request");
    let joined = request
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n---\n");

    assert!(
        joined.contains("Relevant memories"),
        "memory context is injected: {joined}"
    );
    assert!(
        joined.contains("blue green deploy requires warming the standby pool"),
        "the relevant same-workspace memory is present: {joined}"
    );
    assert!(
        joined.contains(&expected_receipt.to_string()),
        "provenance includes receipt id"
    );
    assert!(
        !joined.contains("different workspace must not leak"),
        "cross-workspace memory must stay isolated: {joined}"
    );
}

#[tokio::test]
async fn denied_turn_does_not_recall_or_inject_memory() {
    let provider = Arc::new(EchoProvider::new());
    let memory = Arc::new(InMemoryMemoryRuntime::new());
    memory
        .record(memory_fact(
            HOLDER,
            "secret denied path memory",
            uuid::Uuid::new_v4(),
        ))
        .expect("record memory");

    let runtime =
        support::runtime_builder_with_policy(provider.clone(), support::deny_all_policy())
            .with_memory(memory)
            .build()
            .expect("runtime builds");

    let denied = runtime
        .submit(request_for(
            "secret denied path",
            &valid_token(),
            ardur_runtime::SessionId::new(),
        ))
        .await;

    assert!(denied.is_err(), "Cedar denial should fail the turn");
    assert_eq!(provider.call_count(), 0, "provider was never called");
    assert!(
        provider.last_request().is_none(),
        "no memory context reached provider"
    );
}

#[tokio::test]
async fn recall_respects_configured_k_limit() {
    let provider = Arc::new(EchoProvider::new());
    let memory = Arc::new(InMemoryMemoryRuntime::new());
    for i in 0..4 {
        memory
            .record(memory_fact(
                HOLDER,
                &format!("tea recall candidate {i}"),
                uuid::Uuid::new_v4(),
            ))
            .expect("record candidate");
    }

    let runtime = runtime_builder(provider.clone())
        .with_memory(memory)
        .memory_recall_k(2)
        .build()
        .expect("runtime builds");

    runtime
        .submit(request_for(
            "tea",
            &valid_token(),
            ardur_runtime::SessionId::new(),
        ))
        .await
        .expect("turn succeeds");

    let request = provider.last_request().expect("provider saw request");
    let recall_block = request
        .messages
        .iter()
        .find(|m| m.content.contains("Relevant memories"))
        .map(|m| m.content.as_str())
        .expect("memory context is injected");
    let injected = recall_block
        .lines()
        .filter(|line| line.trim_start().starts_with("- id="))
        .count();
    assert_eq!(
        injected, 2,
        "recall block must obey configured K: {recall_block}"
    );
}

#[tokio::test]
async fn recall_threshold_excludes_weak_same_subject_hits() {
    let provider = Arc::new(EchoProvider::new());
    let memory = Arc::new(InMemoryMemoryRuntime::new());
    memory
        .record(memory_fact_with_confidence(
            HOLDER,
            "blue green deploy requires warming the standby pool",
            0.9,
            uuid::Uuid::new_v4(),
        ))
        .expect("record strong memory");
    memory
        .record(memory_fact_with_confidence(
            HOLDER,
            "blue notebook lives on the kitchen counter",
            0.9,
            uuid::Uuid::new_v4(),
        ))
        .expect("record weak memory");
    memory
        .record(memory_fact_with_confidence(
            HOLDER,
            "blue green deploy with stale confidence should stay out",
            0.2,
            uuid::Uuid::new_v4(),
        ))
        .expect("record low-confidence memory");
    memory
        .record(memory_fact(
            HOLDER,
            "unrelated espresso preference",
            uuid::Uuid::new_v4(),
        ))
        .expect("record irrelevant memory");

    let runtime = runtime_builder(provider.clone())
        .with_memory(memory)
        .memory_recall_threshold(0.67)
        .build()
        .expect("runtime builds");

    runtime
        .submit(request_for(
            "blue green deploy",
            &valid_token(),
            ardur_runtime::SessionId::new(),
        ))
        .await
        .expect("turn succeeds");

    let request = provider.last_request().expect("provider saw request");
    let joined = request
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n---\n");

    assert!(
        joined.contains("blue green deploy requires warming the standby pool"),
        "strong relevant memory is injected: {joined}"
    );
    assert!(
        !joined.contains("blue notebook lives on the kitchen counter"),
        "weak same-subject lexical hit is excluded by threshold: {joined}"
    );
    assert!(
        !joined.contains("stale confidence should stay out"),
        "below-threshold confidence is excluded even when query terms match: {joined}"
    );
    assert!(
        !joined.contains("unrelated espresso preference"),
        "irrelevant same-subject memory is not injected: {joined}"
    );
}
