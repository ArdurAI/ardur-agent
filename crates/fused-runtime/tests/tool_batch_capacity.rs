//! Tool-batch capacity is refused before any tool effect, without poisoning admission.
mod support;
use ardur_cost_gate::{CostEnvelope, CostTuple};
use ardur_fused_runtime::settlement::LIMITS;
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, Provider, ProviderError, ProviderStream,
    RateCard, StreamEvent, Usage,
};
use ardur_runtime::{ChatRuntime, ProviderId, SessionId, ToolCall};
use ardur_session_journals::{
    FileSessionJournal, JournalEntry, SessionJournal,
    settlement::{RefusalClass, SettlementDecision, SettlementPhase},
};
use ardur_tool_registry::{
    Capability, Tool, ToolContext, ToolError, ToolId, ToolOutput, ToolRegistry, ToolSchema,
};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct BatchProvider {
    size: usize,
    calls: AtomicUsize,
    rate: RateCard,
}
#[async_trait]
impl Provider for BatchProvider {
    async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        let cost = if first { 4 } else { 0 };
        Ok(CompletionResponse {
            content: "local".into(),
            finish_reason: if first {
                FinishReason::ToolUse(
                    (0..self.size)
                        .map(|i| ToolCall {
                            id: format!("call_{i}"),
                            name: "batch.echo".into(),
                            arguments: json!({}),
                        })
                        .collect(),
                )
            } else {
                FinishReason::Stop
            },
            usage: Usage {
                cost_cents: Some(cost),
                ..Usage::default()
            },
            cost: CostTuple::cents(cost),
            raw_provider_response: None,
        })
    }
    async fn stream(&self, req: CompletionRequest) -> Result<ProviderStream, ProviderError> {
        let response = self.complete(req).await?;
        Ok(Box::pin(futures::stream::iter([
            Ok(StreamEvent::Usage(response.usage)),
            Ok(StreamEvent::Finish(response.finish_reason)),
        ])))
    }
    fn id(&self) -> ProviderId {
        ProviderId("batch-fixture".into())
    }
    fn supports_streaming(&self) -> bool {
        true
    }
    fn rate_card(&self) -> &RateCard {
        &self.rate
    }
}
struct CountedTool {
    calls: Arc<AtomicUsize>,
    schema: ToolSchema,
}
#[async_trait]
impl Tool for CountedTool {
    fn id(&self) -> ToolId {
        ToolId::new("batch.echo")
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn required_capabilities(&self) -> &[Capability] {
        &[]
    }
    async fn invoke(&self, _: &ToolContext, _: serde_json::Value) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput {
            content: json!({"ok":true}),
            cost: CostTuple::ZERO,
            receipt_data: json!({}),
        })
    }
}
async fn check(streaming: bool, oversized: bool) {
    let dir = support::tempdir().unwrap();
    let session = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(dir.path().join("journals"), session).unwrap());
    let size = LIMITS.max_tools_per_round + usize::from(oversized);
    let provider = Arc::new(BatchProvider {
        size,
        calls: AtomicUsize::new(0),
        rate: RateCard {
            version_id: "local".into(),
            cents_per_1k_input: 0.0,
            cents_per_1k_output: 0.0,
            cents_per_request: 0.0,
        },
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(CountedTool {
            calls: calls.clone(),
            schema: ToolSchema {
                description: "count actual local effects".into(),
                input_schema: json!({"type":"object"}),
                output_schema: json!({"type":"object"}),
                examples: vec![],
            },
        }))
        .unwrap();
    let registry = Arc::new(registry);
    let token = support::mint_token_as(
        support::HOLDER,
        support::AUDIENCE,
        &[support::TOOL, "batch.echo", "memory.write"],
    );
    let build = || {
        support::runtime_builder(provider.clone())
            .with_tools(registry.clone())
            .with_journal(journal.clone())
            .receipt_log(dir.path().join("receipts.jsonl"))
            .projected_envelope(CostEnvelope {
                cents_max: 10,
                ..Default::default()
            })
            .provision_budget(support::gate_holder(), CostTuple::cents(20))
            .build()
            .unwrap()
    };
    let runtime = build();
    let request = support::request_for("batch", &token, session);
    let failed = if streaming {
        let events = runtime.stream(request).collect::<Vec<_>>().await;
        events.iter().any(Result::is_err)
    } else {
        runtime.submit(request).await.is_err()
    };
    assert_eq!(
        calls.load(Ordering::SeqCst),
        if oversized { 0 } else { size },
        "over-capacity batch must cause no tool effect"
    );
    assert_eq!(failed, oversized);
    assert_eq!(
        runtime
            .remaining_budget(&support::gate_holder())
            .await
            .unwrap(),
        CostTuple::cents(16)
    );
    assert!(
        runtime.settlement_supervisor().status().turns.is_empty(),
        "no stranded reservation or quarantine"
    );
    if oversized {
        let snapshots = runtime.settlement_supervisor().durable_snapshots().unwrap();
        assert_eq!(snapshots.len(), 1);
        let round = &snapshots[0].turn().rounds[0];
        assert_eq!(
            round.decision,
            Some(SettlementDecision::Refusal(RefusalClass::Capacity))
        );
        assert!(round.tools.is_empty());
        let SettlementPhase::Settled {
            application,
            receipt: None,
        } = &round.phase
        else {
            panic!("capacity refusal must settle")
        };
        assert_eq!(application.applied_debit, CostTuple::cents(4));
        assert_eq!(round.known_incurred, CostTuple::cents(4));
        let entries = journal.replay(session).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert!(
            matches!(&entries[0], JournalEntry::CostFinalized { actual, reason: Some(reason), .. } if *actual == CostTuple::cents(4) && reason == "refusal:capacity")
        );
        assert!(
            ardur_fused_runtime::load_persisted_chain(dir.path().join("receipts.jsonl"))
                .unwrap()
                .is_empty()
        );
        runtime
            .submit(support::request_for("healthy next", &token, session))
            .await
            .unwrap();
        assert_eq!(
            runtime
                .remaining_budget(&support::gate_holder())
                .await
                .unwrap(),
            CostTuple::cents(16)
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    } else {
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    }
    drop(runtime);
    let reopened = build();
    assert!(
        reopened
            .settlement_supervisor()
            .status()
            .boot_problem
            .is_none()
    );
    assert_eq!(
        reopened
            .remaining_budget(&support::gate_holder())
            .await
            .unwrap(),
        CostTuple::cents(20)
    );
    reopened
        .submit(support::request_for(
            "healthy after restart",
            &token,
            session,
        ))
        .await
        .unwrap();
    assert_eq!(
        reopened
            .remaining_budget(&support::gate_holder())
            .await
            .unwrap(),
        CostTuple::cents(20)
    );
}
#[tokio::test]
async fn oversized_submit_batch_refuses_before_effects() {
    check(false, true).await;
}
#[tokio::test]
async fn oversized_stream_batch_refuses_before_effects() {
    check(true, true).await;
}
#[tokio::test]
async fn at_capacity_submit_batch_remains_usable() {
    check(false, false).await;
}
#[tokio::test]
async fn at_capacity_stream_batch_remains_usable() {
    check(true, false).await;
}
