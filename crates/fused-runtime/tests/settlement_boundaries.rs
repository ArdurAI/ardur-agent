//! Settlement ownership at actual owning-stream drop boundaries.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ardur_cost_gate::{CostEnvelope, CostTuple as GateCostTuple};
use ardur_fused_runtime::{
    FusedEvent, ReceiptChainError, StageKind, load_persisted_chain, verify_persisted_chain,
    verify_persisted_chain_with_jwks,
};
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, Provider, ProviderError, ProviderStream,
    RateCard, StreamEvent, Usage,
};
use ardur_receipt::Jwks;
use ardur_runtime::{CostTuple, ProviderId, SessionId};
use ardur_session_journals::{FileSessionJournal, JournalEntry, SessionJournal};
use async_trait::async_trait;
use futures::StreamExt;
use support::{gate_holder, request_for, runtime_builder, valid_token};

struct KnownUsageProvider {
    calls: AtomicUsize,
    rate_card: RateCard,
    pause_after_usage: bool,
    request_tool: bool,
}

impl KnownUsageProvider {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            pause_after_usage: false,
            request_tool: false,
            rate_card: RateCard {
                version_id: "local-known-usage".into(),
                cents_per_1k_input: 0.0,
                cents_per_1k_output: 0.0,
                cents_per_request: 0.0,
            },
        }
    }
}

#[async_trait]
impl Provider for KnownUsageProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            content: "local completed response".into(),
            finish_reason: FinishReason::Stop,
            usage: Usage {
                tokens_in: 0,
                tokens_out: 0,
                cost_cents: Some(9),
            },
            cost: CostTuple {
                cents: 9,
                ..CostTuple::default()
            },
            raw_provider_response: None,
        })
    }

    async fn stream(&self, _req: CompletionRequest) -> Result<ProviderStream, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.pause_after_usage {
            return Ok(Box::pin(
                futures::stream::iter([Ok(StreamEvent::Usage(Usage {
                    tokens_in: 0,
                    tokens_out: 0,
                    cost_cents: Some(9),
                }))])
                .chain(futures::stream::pending()),
            ));
        }
        Ok(Box::pin(futures::stream::iter([
            Ok(StreamEvent::ContentDelta("local completed response".into())),
            Ok(StreamEvent::Usage(Usage {
                tokens_in: 0,
                tokens_out: 0,
                cost_cents: Some(9),
            })),
            Ok(StreamEvent::Finish(if self.request_tool {
                FinishReason::ToolUse(vec![ardur_runtime::ToolCall {
                    id: "paid-echo".into(),
                    name: "echo".into(),
                    arguments: serde_json::json!({"local":true}),
                }])
            } else {
                FinishReason::Stop
            })),
        ])))
    }

    fn id(&self) -> ProviderId {
        ProviderId("local-known-usage".into())
    }
    fn supports_streaming(&self) -> bool {
        true
    }
    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

#[tokio::test]
async fn dropping_at_admission_success_releases_a_no_work_hold() {
    let provider = Arc::new(KnownUsageProvider::new());
    let runtime = runtime_builder(provider.clone())
        .projected_envelope(CostEnvelope {
            cents_max: 10,
            ..Default::default()
        })
        .provision_budget(gate_holder(), GateCostTuple::cents(10))
        .build()
        .expect("runtime builds");
    let session = SessionId::new();
    let mut stream = Box::pin(runtime.stream(request_for("local", &valid_token(), session)));
    loop {
        let item = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("stream makes progress")
            .expect("admission event exists")
            .expect("admission succeeds");
        if matches!(
            item,
            FusedEvent::StageEnd {
                stage: StageKind::CostGateAdmit,
                ok: true
            }
        ) {
            break;
        }
    }
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        0,
        "no provider dispatch yet"
    );
    assert_eq!(
        runtime
            .remaining_budget(&gate_holder())
            .await
            .unwrap()
            .cents,
        0,
        "the real admission hold was deducted before the yield"
    );
    drop(stream); // Drop Pin<Box<Stream>>, not merely Pin<&mut Stream>.
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime
            .remaining_budget(&gate_holder())
            .await
            .unwrap()
            .cents,
        10,
        "no-work admission-success drop must refund its entire hold"
    );
}

#[tokio::test]
async fn dropping_after_known_usage_preserves_operator_expense_without_caller_debit() {
    let dir = support::tempdir().expect("state dir");
    let session = SessionId::new();
    let journal_root = dir.path().join("journals");
    let receipt_path = dir.path().join("receipts.jsonl");
    let journal = Arc::new(FileSessionJournal::new(&journal_root, session).unwrap());
    let provider = Arc::new(KnownUsageProvider::new());
    let runtime = runtime_builder(provider.clone())
        .with_journal(journal.clone())
        .receipt_log(&receipt_path)
        .projected_envelope(CostEnvelope {
            cents_max: 10,
            ..Default::default()
        })
        .provision_budget(gate_holder(), GateCostTuple::cents(10))
        .build()
        .expect("runtime builds");
    let mut stream = Box::pin(runtime.stream(request_for("local", &valid_token(), session)));
    loop {
        let item = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("stream makes progress")
            .expect("usage event exists")
            .expect("provider succeeds");
        if matches!(
            item,
            FusedEvent::Usage(Usage {
                cost_cents: Some(9),
                ..
            })
        ) {
            break;
        }
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    drop(stream);
    assert_eq!(
        runtime
            .remaining_budget(&gate_holder())
            .await
            .unwrap()
            .cents,
        10,
        "preserve precommit cancellation: no caller debit"
    );
    let supervisor = runtime.settlement_supervisor();
    let snapshots = supervisor.durable_snapshots().unwrap();
    assert_eq!(snapshots.len(), 1);
    let turn = snapshots[0].turn();
    assert_eq!(
        turn.terminal,
        ardur_session_journals::settlement::TurnTerminal::Cancelled
    );
    assert_eq!(turn.rounds[0].known_incurred.cents, 9);
    assert!(matches!(&turn.rounds[0].phase,
        ardur_session_journals::settlement::SettlementPhase::Settled { application, receipt: None }
        if application.applied_debit == CostTuple::ZERO && application.reserved_credit.cents == 10));
    drop(runtime);
    // Drop is synchronous authoritative settlement, NOT async journal persistence.
    // The retained supervisor is explicitly driven before reopening the projection.
    assert_eq!(supervisor.drain_pending(journal.as_ref()).await.unwrap(), 1);
    assert_eq!(supervisor.drain_pending(journal.as_ref()).await.unwrap(), 0);
    assert!(supervisor.status().turns.is_empty());
    drop(journal);
    let reopened = FileSessionJournal::new(&journal_root, session).unwrap();
    let entries = reopened.replay(session).await.unwrap();
    assert!(
        entries.iter().any(|entry| matches!(entry,
        JournalEntry::OperatorExpense { provider_cost, .. } if provider_cost.cents == 9)),
        "owning drop lost the known 9c operator expense instead of recording it durably"
    );
    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry, JournalEntry::AssistantMessage { .. })),
        "a cancelled uncommitted response is not an assistant completion"
    );
    let chain = load_persisted_chain(&receipt_path).unwrap();
    assert!(
        chain.is_empty(),
        "precommit cancellation mints no completion receipt"
    );
}

#[tokio::test]
async fn fully_drained_known_usage_control_is_billed_and_authentically_receipted() {
    let dir = support::tempdir().unwrap();
    let receipt_path = dir.path().join("receipts.jsonl");
    let provider = Arc::new(KnownUsageProvider::new());
    let runtime = runtime_builder(provider.clone())
        .receipt_log(&receipt_path)
        .projected_envelope(CostEnvelope {
            cents_max: 10,
            ..Default::default()
        })
        .provision_budget(gate_holder(), GateCostTuple::cents(10))
        .build()
        .unwrap();
    let session = SessionId::new();
    let mut stream = Box::pin(runtime.stream(request_for("local", &valid_token(), session)));
    let mut receipt_events = 0;
    let mut finishes = 0;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("fully drained control never stalls")
        {
            Some(Ok(FusedEvent::Receipt { cost_cents, .. })) => {
                assert_eq!(cost_cents, 9);
                receipt_events += 1;
            }
            Some(Ok(FusedEvent::Finish(FinishReason::Stop))) => finishes += 1,
            Some(Ok(_)) => {}
            Some(Err(err)) => panic!("control failed: {err:?}"),
            None => break,
        }
    }
    drop(stream);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(receipt_events, 1);
    assert_eq!(finishes, 1, "exactly one final answer before EOF");
    assert_eq!(
        runtime
            .remaining_budget(&gate_holder())
            .await
            .unwrap()
            .cents,
        1
    );
    let chain = load_persisted_chain(&receipt_path).unwrap();
    let jwks = Jwks::from_public_key(&support::receipt_key().public_key());
    verify_persisted_chain_with_jwks(&chain, &jwks).expect("ES256 signatures and linkage verify");
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].body.cost.cents, 9);
    assert_eq!(chain[0].body.session_id, Some(session.0));

    // Negative control: valid payload and genesis linkage cannot authenticate
    // a corrupted signature. Modify only one significant base64url character.
    let original = std::fs::read_to_string(&receipt_path).unwrap();
    let mut forged = original.clone().into_bytes();
    let signature_start = original.rfind('.').unwrap() + 1;
    forged[signature_start] = if forged[signature_start] == b'A' {
        b'B'
    } else {
        b'A'
    };
    let forged_path = dir.path().join("signature-negative-control.jsonl");
    std::fs::write(&forged_path, forged).unwrap();
    let tampered =
        load_persisted_chain(&forged_path).expect("signature change preserves parseable payload");
    verify_persisted_chain(&tampered).expect("one-element hash linkage alone accepts the control");
    assert!(
        matches!(
            verify_persisted_chain_with_jwks(&tampered, &jwks),
            Err(ReceiptChainError::InvalidSignature { at: 0, .. })
        ),
        "authenticated verification must reject the modified signature"
    );
}

#[tokio::test]
async fn internal_usage_is_durable_before_next_provider_poll_and_drop() {
    let dir = support::tempdir().unwrap();
    let mut provider = KnownUsageProvider::new();
    provider.pause_after_usage = true;
    let provider = Arc::new(provider);
    let runtime = runtime_builder(provider.clone())
        .receipt_log(dir.path().join("receipts.jsonl"))
        .projected_envelope(CostEnvelope {
            cents_max: 10,
            ..Default::default()
        })
        .provision_budget(gate_holder(), GateCostTuple::cents(10))
        .build()
        .unwrap();
    let supervisor = runtime.settlement_supervisor();
    let mut stream =
        Box::pin(runtime.stream(request_for("local", &valid_token(), SessionId::new())));
    loop {
        match futures::poll!(stream.next()) {
            std::task::Poll::Ready(Some(Ok(_))) => (),
            std::task::Poll::Pending => break,
            other => panic!("unexpected stream state: {other:?}"),
        }
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let snapshots = supervisor.durable_snapshots().unwrap();
    assert_eq!(snapshots[0].turn().rounds[0].known_incurred.cents, 9);
    drop(stream);
    assert_eq!(
        runtime
            .remaining_budget(&gate_holder())
            .await
            .unwrap()
            .cents,
        10
    );
    assert_eq!(
        supervisor.durable_snapshots().unwrap()[0].turn().rounds[0]
            .known_incurred
            .cents,
        9
    );
}

#[test]
fn production_requires_storage_even_with_test_feature_and_setter_order() {
    let make = || {
        ardur_fused_runtime::FusedRuntimeBuilder::new(
            support::cap_root(),
            support::permissive_policy(),
            Arc::new(KnownUsageProvider::new()),
            support::receipt_key(),
            ardur_provider_runtime::ModelId::new(support::TEST_MODEL),
        )
    };
    assert!(matches!(
        make().build(),
        Err(ReceiptChainError::SettlementLogRequired)
    ));
    assert!(matches!(
        make()
            .require_durable_settlements()
            .test_settlement_storage()
            .build(),
        Err(ReceiptChainError::SettlementLogRequired)
    ));
    assert!(matches!(
        make()
            .test_settlement_storage()
            .require_durable_settlements()
            .build(),
        Err(ReceiptChainError::SettlementLogRequired)
    ));
}

#[tokio::test]
async fn actual_journal_owner_is_independent_of_request_and_subject_holder() {
    let dir = support::tempdir().unwrap();
    let journal_session = SessionId::new();
    let request_session = SessionId::new();
    let journal =
        Arc::new(FileSessionJournal::new(dir.path().join("journals"), journal_session).unwrap());
    let provider = Arc::new(KnownUsageProvider::new());
    let holder = ardur_cost_gate::HolderId("trusted-other-budget".into());
    let runtime = runtime_builder(provider)
        .with_journal(journal.clone())
        .receipt_log(dir.path().join("receipts.jsonl"))
        .projected_envelope(CostEnvelope {
            cents_max: 10,
            ..Default::default()
        })
        .provision_budget(holder.clone(), GateCostTuple::cents(10))
        .build()
        .unwrap();
    let result = runtime
        .submit_with_provisioning(
            request_for("local", &valid_token(), request_session),
            ardur_fused_runtime::PerRequestProvisioning {
                subject: Some(holder.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(result.cost.cents, 9);
    let snapshots = runtime.settlement_supervisor().durable_snapshots().unwrap();
    let turn = snapshots[0].turn();
    assert_eq!(turn.journal_owner, Some(journal_session));
    assert_eq!(turn.request_session, request_session);
    assert_eq!(turn.budget_holder, holder);
    assert_eq!(turn.verified_subject, gate_holder());
    assert!(matches!(
        turn.rounds[0].projection,
        ardur_session_journals::settlement::JournalProjection::Acknowledged(_)
    ));
    assert_eq!(
        journal
            .replay(journal_session)
            .await
            .unwrap()
            .iter()
            .filter(|e| matches!(e, JournalEntry::AssistantMessage { .. }))
            .count(),
        1
    );
    assert_eq!(runtime.drain_pending_settlements().await.unwrap(), 0);
}

#[tokio::test]
async fn tool_result_yield_keeps_verified_tool_and_provider_expense_before_drop() {
    let dir = support::tempdir().unwrap();
    let session = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(dir.path().join("journal"), session).unwrap());
    let mut provider = KnownUsageProvider::new();
    provider.request_tool = true;
    let provider = Arc::new(provider);
    let runtime = runtime_builder(provider.clone())
        .with_journal(journal.clone())
        .receipt_log(dir.path().join("receipts.jsonl"))
        .with_tools(support::paid_registry("echo", CostTuple::cents(2)))
        .projected_envelope(CostEnvelope {
            cents_max: 20,
            ..Default::default()
        })
        .provision_budget(gate_holder(), GateCostTuple::cents(20))
        .build()
        .unwrap();
    let mut stream = Box::pin(runtime.stream(request_for("local", &valid_token(), session)));
    loop {
        if matches!(
            stream.next().await.unwrap().unwrap(),
            FusedEvent::ToolCallResult { .. }
        ) {
            break;
        }
    }
    let supervisor = runtime.settlement_supervisor();
    let snapshots = supervisor.durable_snapshots().unwrap();
    let round = &snapshots[0].turn().rounds[0];
    assert_eq!(round.known_incurred.cents, 11);
    assert!(matches!(round.provider_evidence,
        ardur_session_journals::settlement::ProviderEvidence::Observed { cost, .. } if cost.cents == 9));
    assert!(matches!(round.tools[0].effect,
        ardur_session_journals::settlement::ToolEffect::Completed { cost, .. } if cost.cents == 2));
    drop(stream);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime
            .remaining_budget(&gate_holder())
            .await
            .unwrap()
            .cents,
        20
    );
    assert_eq!(runtime.drain_pending_settlements().await.unwrap(), 1);
    let entries = journal.replay(session).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert!(
        matches!(entries[0], JournalEntry::OperatorExpense { provider_cost, .. } if provider_cost.cents == 11)
    );
    assert!(
        load_persisted_chain(dir.path().join("receipts.jsonl"))
            .unwrap()
            .is_empty()
    );
}
