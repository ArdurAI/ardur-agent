//! Bounded modern recovery: signed bindings and journal ownership, never budget replay.
mod support;
use ardur_fused_runtime::{
    ReconciliationAction, load_persisted_chain, verify_persisted_chain_with_jwks,
};
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, Provider, ProviderError, RateCard, Usage,
};
use ardur_runtime::{ChatRuntime, CostTuple, ProviderId, Role, SessionId, ToolCall};
use ardur_session_journals::{
    EntryId, FileSessionJournal, InMemorySessionJournal, JournalEntry, JournalError, SessionJournal,
};
use async_trait::async_trait;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use support::*;

struct FailAnswer {
    inner: Arc<FileSessionJournal>,
    after_write: bool,
    fail_cost: bool,
}
#[async_trait]
impl SessionJournal for FailAnswer {
    async fn append(&self, entry: JournalEntry) -> Result<EntryId, JournalError> {
        let fail = if self.fail_cost {
            matches!(entry, JournalEntry::CostFinalized { .. })
        } else {
            matches!(entry, JournalEntry::AssistantMessage { .. })
        };
        if fail {
            if self.after_write {
                self.inner.append(entry).await?;
            }
            return Err(JournalError::Io(std::io::Error::other(
                "controlled lost journal acknowledgement",
            )));
        }
        self.inner.append(entry).await
    }
    async fn replay(&self, id: SessionId) -> Result<Vec<JournalEntry>, JournalError> {
        self.inner.replay(id).await
    }
    async fn replay_from(
        &self,
        id: SessionId,
        from: EntryId,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        self.inner.replay_from(id, from).await
    }
    async fn close(&self) -> Result<(), JournalError> {
        self.inner.close().await
    }
    fn session_id(&self) -> &SessionId {
        self.inner.session_id()
    }
}
fn verify(log: &std::path::Path) {
    verify_persisted_chain_with_jwks(
        &load_persisted_chain(log).unwrap(),
        &ardur_receipt::Jwks::from_public_key(&receipt_key().public_key()),
    )
    .unwrap();
}
#[tokio::test]
async fn postreceipt_failures_recover_only_missing_answer_and_never_clear_unknown_projection() {
    for (after_write, fail_cost) in [(false, false), (true, false), (false, true), (true, true)] {
        let root = tempdir().unwrap();
        let log = root.path().join("receipts.jsonl");
        let owner = SessionId::new();
        let request = SessionId::new();
        let journal = Arc::new(FileSessionJournal::new(root.path(), owner).unwrap());
        let runtime = runtime_builder(Arc::new(BillingProvider::new(7)))
            .receipt_log(&log)
            .with_journal(Arc::new(FailAnswer {
                inner: journal.clone(),
                after_write,
                fail_cost,
            }))
            .build()
            .unwrap();
        assert!(
            runtime
                .submit(request_for("original", &valid_token(), request))
                .await
                .is_err()
        );
        let receipts = load_persisted_chain(&log).unwrap();
        assert_eq!(receipts.len(), 1);
        let id = receipts[0].body.receipt_id;
        let before = journal.replay(owner).await.unwrap();
        let snapshot = runtime.settlement_supervisor().durable_snapshots().unwrap();
        assert!(
            matches!(snapshot[0].turn().terminal, ardur_session_journals::settlement::TurnTerminal::FinalAnswer(r) if r.0 == id)
        );
        drop(runtime);
        let runtime = runtime_builder(Arc::new(EchoProvider::new()))
            .receipt_log(&log)
            .with_journal(journal.clone())
            .build()
            .unwrap();
        let budget = runtime.remaining_budget(&gate_holder()).await;
        let expected = usize::from(fail_cost || !after_write);
        assert_eq!(
            runtime
                .reconcile_receipts(true)
                .await
                .unwrap()
                .orphan_receipt_count(),
            expected
        );
        assert_eq!(
            journal.replay(owner).await.unwrap(),
            before,
            "dry run writes nothing"
        );
        let report = runtime.reconcile_receipts(false).await.unwrap();
        assert_eq!(report.orphan_receipt_count(), expected);
        let after = journal.replay(owner).await.unwrap();
        assert_eq!(after.iter().filter(|e| matches!(e, JournalEntry::AssistantMessage { receipt_id, .. } if receipt_id.0 == id)).count(), 1);
        assert_eq!(
            runtime.reconcile_receipts(false).await.unwrap().action,
            ReconciliationAction::NoOrphans
        );
        assert_eq!(runtime.remaining_budget(&gate_holder()).await, budget);
        assert_eq!(
            runtime
                .settlement_supervisor()
                .status()
                .boot_problem
                .is_some(),
            fail_cost
        );
        if fail_cost {
            assert!(
                runtime
                    .submit(request_for("not admitted", &valid_token(), request))
                    .await
                    .is_err()
            );
            assert_eq!(runtime.remaining_budget(&gate_holder()).await, budget);
        }
        verify(&log);
    }
}

#[tokio::test]
async fn foreign_journal_is_not_claimed_even_when_request_session_matches_active_owner() {
    let root = tempdir().unwrap();
    let log = root.path().join("chain.jsonl");
    let owner = SessionId::new();
    let foreign = Arc::new(InMemorySessionJournal::new(SessionId::new()));
    let runtime = runtime_builder(Arc::new(EchoProvider::new()))
        .receipt_log(&log)
        .with_journal(foreign)
        .build()
        .unwrap();
    runtime
        .submit(request_for("foreign", &valid_token(), owner))
        .await
        .unwrap();
    drop(runtime);
    let journal = Arc::new(InMemorySessionJournal::new(owner));
    let (runtime, report) = runtime_builder(Arc::new(EchoProvider::new()))
        .receipt_log(&log)
        .with_journal(journal.clone())
        .build_reconciled()
        .await
        .unwrap();
    assert_eq!(report.orphan_receipt_count(), 0);
    assert!(journal.replay(owner).await.unwrap().is_empty());
    assert_eq!(
        runtime.reconcile_receipts(false).await.unwrap().action,
        ReconciliationAction::NoOrphans
    );
    verify(&log);
}

struct FailedTool {
    schema: ardur_tool_registry::ToolSchema,
}
#[async_trait]
impl ardur_tool_registry::Tool for FailedTool {
    fn id(&self) -> ardur_tool_registry::ToolId {
        ardur_tool_registry::ToolId::new("boom")
    }
    fn schema(&self) -> &ardur_tool_registry::ToolSchema {
        &self.schema
    }
    fn required_capabilities(&self) -> &[ardur_tool_registry::Capability] {
        &[]
    }
    async fn invoke(
        &self,
        _: &ardur_tool_registry::ToolContext,
        _: serde_json::Value,
    ) -> Result<ardur_tool_registry::ToolOutput, ardur_tool_registry::ToolError> {
        Err(ardur_tool_registry::ToolError::ExecutionFailed(
            "controlled failed effect".into(),
        ))
    }
}
struct Rounds {
    cancel: Arc<AtomicBool>,
    mode: &'static str,
    rates: RateCard,
}
#[async_trait]
impl Provider for Rounds {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let next = req.messages.iter().any(|m| m.role == Role::Tool);
        if next && self.mode == "cancel" {
            self.cancel.store(true, Ordering::SeqCst);
        }
        let finish_reason = if !next || self.mode == "exhaust" {
            FinishReason::ToolUse(vec![ToolCall {
                id: "echo-call".into(),
                name: "echo".into(),
                arguments: json!({"text":"known output"}),
            }])
        } else if self.mode == "failed" {
            FinishReason::ToolUse(vec![ToolCall {
                id: "failed-call".into(),
                name: "boom".into(),
                arguments: json!({}),
            }])
        } else if self.mode == "refuse" {
            FinishReason::ToolUse(vec![ToolCall {
                id: "missing-call".into(),
                name: "missing".into(),
                arguments: json!({}),
            }])
        } else {
            FinishReason::Stop
        };
        Ok(CompletionResponse {
            content: "not recoverable text".into(),
            finish_reason,
            usage: Usage::default(),
            cost: CostTuple {
                cents: 2,
                ..Default::default()
            },
            raw_provider_response: None,
        })
    }
    fn id(&self) -> ProviderId {
        ProviderId("recovery-rounds".into())
    }
    fn supports_streaming(&self) -> bool {
        false
    }
    fn rate_card(&self) -> &RateCard {
        &self.rates
    }
}
#[tokio::test]
async fn intermediate_successes_recover_as_tools_not_answers_for_final_cancel_refusal_and_exhaustion()
 {
    for mode in ["final", "failed", "cancel", "refuse", "exhaust"] {
        let root = tempdir().unwrap();
        let log = root.path().join("chain.jsonl");
        let owner = SessionId::new();
        let session = SessionId::new();
        let cancel = Arc::new(AtomicBool::new(false));
        let provider = Arc::new(Rounds {
            cancel: cancel.clone(),
            mode,
            rates: RateCard::anthropic_2026_q2_v1(),
        });
        let journal = Arc::new(InMemorySessionJournal::new(owner));
        let mut tools = ardur_tool_registry::ToolRegistry::new();
        tools
            .register(Box::new(PaidTool::new(
                "echo",
                CostTuple {
                    cents: 3,
                    ..Default::default()
                },
            )))
            .unwrap();
        tools
            .register(Box::new(FailedTool {
                schema: ardur_tool_registry::ToolSchema {
                    description: "controlled failure".into(),
                    input_schema: json!({"type":"object"}),
                    output_schema: json!({}),
                    examples: vec![],
                },
            }))
            .unwrap();
        let runtime = runtime_builder(provider)
            .receipt_log(&log)
            .with_journal(journal)
            .with_tools(Arc::new(tools))
            .projected_envelope(ardur_cost_gate::CostEnvelope {
                tokens_in_max: 1000,
                tokens_out_max: 1000,
                cents_max: 1000,
                wall_ms_max: 1000,
                attention_score_max: 1000,
            })
            .max_tool_iterations(if mode == "exhaust" { 2 } else { 3 })
            .build()
            .unwrap();
        let result = runtime
            .submit_with_cancellation(
                request_for("rounds", &valid_token(), session),
                Default::default(),
                Arc::new(move || cancel.load(Ordering::SeqCst)),
                None,
            )
            .await;
        assert_eq!(result.is_ok(), mode == "final", "{mode}: {result:?}");
        let chain_before = std::fs::read(&log).unwrap();
        drop(runtime);
        // Discard only derived journal entries, preserving authoritative ownership.
        let journal = Arc::new(InMemorySessionJournal::new(owner));
        let (runtime, _) = runtime_builder(Arc::new(EchoProvider::new()))
            .receipt_log(&log)
            .with_journal(journal.clone())
            .build_reconciled()
            .await
            .unwrap();
        let entries = journal.replay(owner).await.unwrap();
        assert_eq!(
            entries
                .iter()
                .filter(|e| matches!(e, JournalEntry::AssistantMessage { .. }))
                .count(),
            usize::from(mode == "final")
        );
        assert_eq!(entries.iter().filter(|e| matches!(e, JournalEntry::ToolInvocation { tool_id, .. } if tool_id.0 == "echo")).count(), 1,
            "only the observed successful tool is recovered; an exhausted not-invoked request is not success");
        assert!(
            !entries.iter().any(
                |e| matches!(e, JournalEntry::ToolInvocation { tool_id, .. } if tool_id.0 != "echo")
            ),
            "failed or uninvoked tools are not recovered as successful"
        );
        assert_eq!(
            runtime.reconcile_receipts(false).await.unwrap().action,
            ReconciliationAction::NoOrphans
        );
        assert_eq!(std::fs::read(&log).unwrap(), chain_before);
        verify(&log);
    }
}

#[tokio::test]
async fn before_receipt_failure_cannot_be_recovered_as_completion() {
    let root = tempdir().unwrap();
    let log = root.path().join("chain.jsonl");
    let owner = SessionId::new();
    let runtime = runtime_builder(Arc::new(BillingProvider::new(7)))
        .receipt_log(&log)
        .with_journal(Arc::new(InMemorySessionJournal::new(owner)))
        .build()
        .unwrap();
    std::fs::remove_file(&log).unwrap();
    std::fs::create_dir(&log).unwrap();
    assert!(
        runtime
            .submit(request_for(
                "fails before receipt preparation",
                &valid_token(),
                SessionId::new()
            ))
            .await
            .is_err()
    );
    drop(runtime);
    std::fs::remove_dir(&log).unwrap();
    let journal = Arc::new(InMemorySessionJournal::new(owner));
    let (runtime, report) = runtime_builder(Arc::new(EchoProvider::new()))
        .receipt_log(&log)
        .with_journal(journal.clone())
        .build_reconciled()
        .await
        .unwrap();
    assert_eq!(report.orphan_receipt_count(), 0);
    assert!(journal.replay(owner).await.unwrap().is_empty());
    assert!(load_persisted_chain(&log).unwrap().is_empty());
    assert_eq!(
        runtime.remaining_budget(&gate_holder()).await,
        Some(generous_budget())
    );
}
