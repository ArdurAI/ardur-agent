//! Policy-gate regressions: cancellation precedence and ambiguous append.
//! These use local counted tools and a real file journal, never a live provider.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ardur_approvals::{ApprovalStatus, ApprovalStore, Decision, InvocationResult};
use ardur_cost_gate::{CostEnvelope, CostTuple as GateCostTuple};
use ardur_fused_runtime::{CancelProbe, FusedEvent, StageKind, load_persisted_chain};
use ardur_injection_defense::{
    FilterError, FilterId, FilterRegistry, InjectionFilter, ScanResult, ScannableContent, Verdict,
};
use ardur_lifecycle_hooks::{
    ErrorCtx, HookError, HookEvent, HookId, HookRegistry, LifecycleHook, LifecyclePhase,
    PostReceiptCtx, RecordingHook,
};
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, Provider, ProviderError, ProviderStream,
    RateCard, StreamEvent, Usage,
};
use ardur_runtime::{ChatRuntime, CostTuple, ProviderId, RuntimeError, SessionId, ToolCall};
use ardur_session_journals::{
    EntryId, FileSessionJournal, JournalEntry, JournalError, SessionJournal,
};
use ardur_tool_registry::{
    Capability, Tool, ToolContext, ToolError, ToolId, ToolOutput, ToolRegistry, ToolSchema,
};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::json;
use support::{gate_holder, request_for, runtime_builder, valid_token};

struct PaidToolRequest {
    tool: &'static str,
    calls: AtomicUsize,
    rate: RateCard,
    cost: CostTuple,
}

impl PaidToolRequest {
    fn new(tool: &'static str) -> Self {
        Self {
            tool,
            calls: AtomicUsize::new(0),
            rate: RateCard::anthropic_2026_q2_v1(),
            cost: CostTuple::cents(4),
        }
    }
}

#[async_trait]
impl Provider for PaidToolRequest {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            content: String::new(),
            finish_reason: FinishReason::ToolUse(vec![ToolCall {
                id: "local-request".into(),
                name: self.tool.into(),
                arguments: json!({}),
            }]),
            usage: Usage {
                cost_cents: Some(self.cost.cents),
                ..Default::default()
            },
            cost: self.cost,
            raw_provider_response: None,
        })
    }
    fn id(&self) -> ProviderId {
        ProviderId("local-paid-tool-request".into())
    }
    fn supports_streaming(&self) -> bool {
        false
    }
    fn rate_card(&self) -> &RateCard {
        &self.rate
    }
}

#[tokio::test]
async fn already_cancelled_turn_skips_provider_dispatch() {
    let state = support::tempdir().unwrap();
    let session = SessionId::new();
    let receipt_path = state.path().join("receipts.jsonl");
    let journal =
        Arc::new(FileSessionJournal::new(state.path().join("journals"), session).unwrap());
    let provider = Arc::new(PaidToolRequest::new("boom"));
    let runtime = runtime_builder(provider.clone())
        .with_journal(journal.clone())
        .receipt_log(&receipt_path)
        .projected_envelope(CostEnvelope {
            cents_max: 10,
            ..Default::default()
        })
        .provision_budget(gate_holder(), GateCostTuple::cents(10))
        .build()
        .unwrap();
    let result = runtime
        .submit_with_cancellation(
            request_for("local", &valid_token(), session),
            Default::default(),
            Arc::new(|| true),
            None,
        )
        .await;
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        0,
        "an already-cancelled caller must not incur a provider call"
    );
    assert!(matches!(result, Err(RuntimeError::TurnCancelled)));
    assert_eq!(
        runtime
            .remaining_budget(&gate_holder())
            .await
            .unwrap()
            .cents,
        10
    );
    assert!(load_persisted_chain(&receipt_path).unwrap().is_empty());
    assert!(
        journal.replay(session).await.unwrap().is_empty(),
        "no work means no invented operator expense"
    );
}

async fn cancellation_expense_dimension(cost: CostTuple) {
    let state = support::tempdir().unwrap();
    let session = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(state.path(), session).unwrap());
    let provider = Arc::new(PaidToolRequest {
        cost,
        ..PaidToolRequest::new("boom")
    });
    let runtime = runtime_builder(provider.clone())
        .with_journal(journal.clone())
        .build()
        .unwrap();
    let dispatched = provider.clone();
    let result = runtime
        .submit_with_cancellation(
            request_for("local", &valid_token(), session),
            Default::default(),
            Arc::new(move || dispatched.calls.load(Ordering::SeqCst) > 0),
            None,
        )
        .await;
    assert!(matches!(result, Err(RuntimeError::TurnCancelled)));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime.remaining_budget(&gate_holder()).await.unwrap(),
        support::generous_budget()
    );
    let records = journal.replay(session).await.unwrap();
    assert_eq!(
        records.len(),
        1,
        "nonzero {cost:?} is known expense, not free work"
    );
    assert!(
        matches!(&records[0], JournalEntry::OperatorExpense { provider_cost, .. } if *provider_cost == cost)
    );
}

#[tokio::test]
async fn cancellation_preserves_input_token_only_expense() {
    cancellation_expense_dimension(CostTuple {
        tokens_in: 3,
        ..Default::default()
    })
    .await;
}

#[tokio::test]
async fn cancellation_preserves_wall_time_only_expense() {
    cancellation_expense_dimension(CostTuple {
        wall_ms: 7,
        ..Default::default()
    })
    .await;
}

#[tokio::test]
async fn cancellation_preserves_attention_only_expense() {
    cancellation_expense_dimension(CostTuple {
        attention_score: 11,
        ..Default::default()
    })
    .await;
}

// The entry predicate already preserved output tokens: this is an honest
// preservation control, not a newly missing-behavior RED.
#[tokio::test]
async fn cancellation_preserves_output_token_only_expense() {
    cancellation_expense_dimension(CostTuple {
        tokens_out: 5,
        ..Default::default()
    })
    .await;
}

async fn cancelled_first_turn_security_control(valid: bool) {
    let state = support::tempdir().unwrap();
    let session = SessionId::new();
    let receipt_path = state.path().join("receipts.jsonl");
    let journal =
        Arc::new(FileSessionJournal::new(state.path().join("journals"), session).unwrap());
    let provider = Arc::new(PaidToolRequest::new("boom"));
    let runtime =
        support::runtime_builder_with_policy(provider.clone(), support::deny_all_policy())
            .with_journal(journal.clone())
            .receipt_log(&receipt_path)
            .build()
            .unwrap();
    let token = if valid {
        valid_token()
    } else {
        "not-a-cap-token".into()
    };
    let result = runtime
        .submit_with_cancellation(
            request_for("local", &token, session),
            Default::default(),
            Arc::new(|| true),
            None,
        )
        .await;
    if valid {
        assert!(
            matches!(result, Err(RuntimeError::PolicyDenied { .. })),
            "{result:?}"
        );
    } else {
        assert!(
            matches!(result, Err(RuntimeError::CapDenied { .. })),
            "{result:?}"
        );
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime.remaining_budget(&gate_holder()).await.unwrap(),
        support::generous_budget()
    );
    assert!(journal.replay(session).await.unwrap().is_empty());
    assert!(load_persisted_chain(&receipt_path).unwrap().is_empty());
}

#[tokio::test]
async fn first_turn_auth_denial_precedes_cancellation_preservation() {
    cancelled_first_turn_security_control(false).await;
}
#[tokio::test]
async fn first_turn_policy_denial_precedes_cancellation_preservation() {
    cancelled_first_turn_security_control(true).await;
}

fn verified_request_claims(
    request: &ardur_runtime::SubmitRequest,
) -> ardur_cap_token::VerifiedClaims {
    use ardur_cap_token::{
        BiscuitCapTokenVerifier, CapToken, CapTokenVerifier, HashSetDenyList, RequiredCaveats,
    };
    let root = support::cap_root();
    let token = CapToken::from_base64(&request.cap_token.0, &root).unwrap();
    BiscuitCapTokenVerifier::new(HashSetDenyList::new())
        .verify(
            &token,
            &root,
            &RequiredCaveats {
                now_unix: support::NOW_UNIX,
                audience: support::AUDIENCE.into(),
                tool: support::TOOL.into(),
                cost: 1,
            },
        )
        .unwrap()
}

fn assert_marker_attribution(
    body: &ardur_receipt::ReceiptBody,
    request_session: SessionId,
    claims: &ardur_cap_token::VerifiedClaims,
) {
    assert_eq!(body.session_id, Some(request_session.0));
    assert_eq!(body.subject.0, claims.subject.0);
    assert_eq!(body.cap_token_id.0, claims.token_id);
}

struct CancelAfterReceipt(Arc<AtomicBool>);

#[async_trait]
impl LifecycleHook for CancelAfterReceipt {
    async fn on_post_receipt(&self, _ctx: &PostReceiptCtx<'_>) -> Result<(), HookError> {
        self.0.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn hook_id(&self) -> HookId {
        HookId::new("cancel-after-receipt")
    }
}

#[tokio::test]
async fn already_cancelled_later_round_skips_dispatch_and_preserves_prior_charge() {
    let state = support::tempdir().unwrap();
    let session = SessionId::new();
    let receipt_path = state.path().join("receipts.jsonl");
    let journal =
        Arc::new(FileSessionJournal::new(state.path().join("journals"), session).unwrap());
    let flag = Arc::new(AtomicBool::new(false));
    let mut hooks = HookRegistry::new();
    hooks.register(Arc::new(CancelAfterReceipt(flag.clone())));
    let provider = Arc::new(PaidToolRequest::new("echo"));
    let runtime = runtime_builder(provider.clone())
        .registry(Arc::new(hooks))
        .with_tools(support::paid_registry("echo", CostTuple::cents(2)))
        .with_journal(journal.clone())
        .receipt_log(&receipt_path)
        .projected_envelope(CostEnvelope {
            cents_max: 10,
            ..Default::default()
        })
        .provision_budget(gate_holder(), GateCostTuple::cents(10))
        .build()
        .unwrap();
    let request = request_for("local", &valid_token(), session);
    let expected_session = request.session_id;
    let expected_claims = verified_request_claims(&request);
    let result = runtime
        .submit_with_cancellation(
            request,
            Default::default(),
            Arc::new(move || flag.load(Ordering::SeqCst)),
            None,
        )
        .await;
    assert!(
        matches!(result, Err(RuntimeError::TurnCancelled)),
        "cancellation wins before next-round admission, never an intermediate success: {result:?}"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime
            .remaining_budget(&gate_holder())
            .await
            .unwrap()
            .cents,
        4
    );
    let chain = load_persisted_chain(&receipt_path).unwrap();
    ardur_fused_runtime::verify_persisted_chain_with_jwks(
        &chain,
        &ardur_receipt::Jwks::from_public_key(&support::receipt_key().public_key()),
    )
    .unwrap();
    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].body.cost.cents, 6);
    assert_eq!(chain[1].body.verb.as_str(), "llm.completion.cancelled.v1");
    assert_eq!(chain[1].body.cost, CostTuple::ZERO);
    assert_marker_attribution(&chain[1].body, expected_session, &expected_claims);
    assert!(
        !journal
            .replay(session)
            .await
            .unwrap()
            .iter()
            .any(|e| matches!(e, JournalEntry::OperatorExpense { .. })),
        "no new provider work to expense"
    );
}

/// The cancellation signal originates in an actual failing provider call.
/// No usage is invented for ProviderError, which carries no cost tuple.
struct CancelThenProviderError {
    cancel: Arc<AtomicBool>,
    cancel_on_error: bool,
    prior_round: bool,
    calls: AtomicUsize,
    paid: PaidToolRequest,
}

#[async_trait]
impl Provider for CancelThenProviderError {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.prior_round && call == 0 {
            return self.paid.complete(request).await;
        }
        if self.cancel_on_error {
            self.cancel.store(true, Ordering::SeqCst);
        }
        Err(ProviderError::Upstream("local provider failure".into()))
    }
    fn id(&self) -> ProviderId {
        ProviderId("cancel-then-provider-error".into())
    }
    fn supports_streaming(&self) -> bool {
        false
    }
    fn rate_card(&self) -> &RateCard {
        self.paid.rate_card()
    }
}

fn counted_tool(
    calls: Arc<AtomicUsize>,
    succeed: bool,
    gated: bool,
    time_out: bool,
) -> Arc<ToolRegistry> {
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(CancelThenFail {
            cancel: Arc::new(AtomicBool::new(false)),
            cancel_in_tool: false,
            time_out,
            succeed,
            gated,
            calls,
            schema: ToolSchema {
                description: "local counted boundary".into(),
                input_schema: json!({"type":"object"}),
                output_schema: json!({"type":"object"}),
                examples: vec![],
            },
        }))
        .unwrap();
    Arc::new(tools)
}

async fn provider_error_case(cancel_on_error: bool, prior_round: bool) {
    let state = support::tempdir().unwrap();
    let session = SessionId::new();
    let receipt_path = state.path().join("receipts.jsonl");
    let journal =
        Arc::new(FileSessionJournal::new(state.path().join("journals"), session).unwrap());
    let store = ApprovalStore::new(state.path().join("approvals"));
    let approval = prior_round.then(|| seed_approved_card(&store, session));
    let flag = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(CancelThenProviderError {
        cancel: flag.clone(),
        cancel_on_error,
        prior_round,
        calls: AtomicUsize::new(0),
        paid: PaidToolRequest::new("boom"),
    });
    let runtime = runtime_builder(provider.clone())
        .with_tools(counted_tool(calls.clone(), true, true, false))
        .with_approvals(store.clone())
        .with_approval_gated_capabilities(["cap.shell_exec".to_string()].into())
        .with_journal(journal.clone())
        .receipt_log(&receipt_path)
        .projected_envelope(CostEnvelope {
            cents_max: 10,
            ..Default::default()
        })
        .provision_budget(gate_holder(), GateCostTuple::cents(20))
        .build()
        .unwrap();
    let request = request_for("local", &valid_token(), session);
    let expected_session = request.session_id;
    let expected_claims = verified_request_claims(&request);
    let result = runtime
        .submit_with_cancellation(
            request,
            Default::default(),
            Arc::new(move || flag.load(Ordering::SeqCst)),
            None,
        )
        .await;
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        if prior_round { 2 } else { 1 }
    );
    assert_eq!(calls.load(Ordering::SeqCst), usize::from(prior_round));
    assert_eq!(
        runtime.remaining_budget(&gate_holder()).await.unwrap(),
        GateCostTuple::cents(if prior_round { 14 } else { 20 }),
        "only the returned current hold is refunded, exactly once"
    );
    if let Some(id) = approval {
        let card = store.read(&id).unwrap();
        assert_eq!(card.status, ApprovalStatus::Consumed);
        assert_eq!(
            card.invocation_outcome.unwrap().result,
            InvocationResult::Completed
        );
    }
    let records = journal.replay(session).await.unwrap();
    assert!(
        !records
            .iter()
            .any(|e| matches!(e, JournalEntry::OperatorExpense { .. })),
        "ProviderError supplied no new cost"
    );
    let chain = load_persisted_chain(&receipt_path).unwrap();
    ardur_fused_runtime::verify_persisted_chain_with_jwks(
        &chain,
        &ardur_receipt::Jwks::from_public_key(&support::receipt_key().public_key()),
    )
    .unwrap();
    if cancel_on_error {
        assert!(
            matches!(result, Err(RuntimeError::TurnCancelled)),
            "{result:?}"
        );
    } else {
        assert!(
            matches!(result, Err(RuntimeError::ProviderUnavailable)),
            "{result:?}"
        );
    }
    assert_eq!(
        chain.len(),
        if prior_round {
            1 + usize::from(cancel_on_error)
        } else {
            0
        }
    );
    if prior_round {
        assert_eq!(chain[0].body.cost, CostTuple::cents(6));
        if cancel_on_error {
            assert_eq!(chain[1].body.verb.as_str(), "llm.completion.cancelled.v1");
            assert_eq!(chain[1].body.cost, CostTuple::ZERO);
            assert_marker_attribution(&chain[1].body, expected_session, &expected_claims);
        }
    }
}

#[tokio::test]
async fn first_provider_error_after_cancellation_refunds_without_expense() {
    provider_error_case(true, false).await;
}
#[tokio::test]
async fn later_provider_error_after_cancellation_terminalizes_prior_charge() {
    provider_error_case(true, true).await;
}
#[tokio::test]
async fn first_provider_error_present_caller_control() {
    provider_error_case(false, false).await;
}
#[tokio::test]
async fn later_provider_error_present_caller_control() {
    provider_error_case(false, true).await;
}

struct CancelInLaterOutboundScan {
    cancel: Arc<AtomicBool>,
    cancel_in_scan: bool,
    scan_error: bool,
    calls: AtomicUsize,
}

#[async_trait]
impl InjectionFilter for CancelInLaterOutboundScan {
    async fn scan(&self, content: &ScannableContent) -> Result<ScanResult, FilterError> {
        if matches!(content, ScannableContent::UserMessage { .. }) {
            // Count the real outbound boundary, not cancellation probe ordinals.
            let previous = self.calls.fetch_add(1, Ordering::SeqCst);
            if previous == 1 {
                if self.cancel_in_scan {
                    self.cancel.store(true, Ordering::SeqCst);
                }
                if self.scan_error {
                    return Err(FilterError::InvalidInput(
                        "later outbound scan failure".into(),
                    ));
                }
            }
        }
        Ok(ScanResult {
            verdict: Verdict::Allow,
            flags: vec![],
            confidence: 1.0,
            scan_duration_ms: 0,
        })
    }
    fn filter_id(&self) -> FilterId {
        FilterId::new("later-outbound-boundary")
    }
    fn confidence_threshold(&self) -> f32 {
        0.5
    }
}

async fn later_early_exit_case(cancel_in_scan: bool, scan_error: bool) {
    let state = support::tempdir().unwrap();
    let session = SessionId::new();
    let receipt_path = state.path().join("receipts.jsonl");
    let journal =
        Arc::new(FileSessionJournal::new(state.path().join("journals"), session).unwrap());
    let flag = Arc::new(AtomicBool::new(false));
    let filter = Arc::new(CancelInLaterOutboundScan {
        cancel: flag.clone(),
        cancel_in_scan,
        scan_error,
        calls: AtomicUsize::new(0),
    });
    let filters = FilterRegistry::new();
    filters.register(filter.clone());
    let store = ApprovalStore::new(state.path().join("approvals"));
    let id = seed_approved_card(&store, session);
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(PaidToolRequest::new("boom"));
    let runtime = runtime_builder(provider.clone())
        .with_tools(counted_tool(calls.clone(), true, true, false))
        .with_approvals(store.clone())
        .with_approval_gated_capabilities(["cap.shell_exec".to_string()].into())
        .with_injection_filters(filters)
        .with_journal(journal.clone())
        .receipt_log(&receipt_path)
        .projected_envelope(CostEnvelope {
            cents_max: 10,
            ..Default::default()
        })
        .provision_budget(gate_holder(), GateCostTuple::cents(10))
        .build()
        .unwrap();
    let request = request_for("local", &valid_token(), session);
    let expected_session = request.session_id;
    let expected_claims = verified_request_claims(&request);
    let result = runtime
        .submit_with_cancellation(
            request,
            Default::default(),
            Arc::new(move || flag.load(Ordering::SeqCst)),
            None,
        )
        .await;
    assert_eq!(filter.calls.load(Ordering::SeqCst), 2);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime.remaining_budget(&gate_holder()).await.unwrap(),
        GateCostTuple::cents(4),
        "no current hold to release; prior debit must remain"
    );
    let card = store.read(&id).unwrap();
    assert_eq!(card.status, ApprovalStatus::Consumed);
    assert_eq!(
        card.invocation_outcome.unwrap().result,
        InvocationResult::Completed
    );
    assert!(
        !journal
            .replay(session)
            .await
            .unwrap()
            .iter()
            .any(|r| matches!(r, JournalEntry::OperatorExpense { .. }))
    );
    if cancel_in_scan {
        assert!(
            matches!(result, Err(RuntimeError::TurnCancelled)),
            "{result:?}"
        );
    } else if scan_error {
        assert!(
            matches!(&result, Err(RuntimeError::Internal(e)) if e.to_string().contains("later outbound scan failure")),
            "{result:?}"
        );
    } else {
        assert!(
            matches!(result, Err(RuntimeError::CostCeilingExceeded)),
            "{result:?}"
        );
    }
    let chain = load_persisted_chain(&receipt_path).unwrap();
    ardur_fused_runtime::verify_persisted_chain_with_jwks(
        &chain,
        &ardur_receipt::Jwks::from_public_key(&support::receipt_key().public_key()),
    )
    .unwrap();
    assert_eq!(chain.len(), 1 + usize::from(cancel_in_scan));
    assert_eq!(chain[0].body.cost, CostTuple::cents(6));
    if cancel_in_scan {
        assert_eq!(chain[1].body.verb.as_str(), "llm.completion.cancelled.v1");
        assert_eq!(chain[1].body.cost, CostTuple::ZERO);
        assert_marker_attribution(&chain[1].body, expected_session, &expected_claims);
    }
}

#[tokio::test]
async fn later_outbound_scan_error_after_cancellation_terminalizes_prior_charge() {
    later_early_exit_case(true, true).await;
}
#[tokio::test]
async fn later_outbound_scan_error_present_caller_control() {
    later_early_exit_case(false, true).await;
}

#[tokio::test]
async fn later_admission_error_after_cancellation_terminalizes_prior_charge() {
    // The later scan returns Allow after setting cancellation; the earlier
    // six-cent debit leaves only four cents for a ten-cent admission.
    later_early_exit_case(true, false).await;
}
#[tokio::test]
async fn later_admission_error_present_caller_control() {
    later_early_exit_case(false, false).await;
}

struct CancelThenFail {
    cancel: Arc<AtomicBool>,
    cancel_in_tool: bool,
    time_out: bool,
    succeed: bool,
    gated: bool,
    calls: Arc<AtomicUsize>,
    schema: ToolSchema,
}

#[async_trait]
impl Tool for CancelThenFail {
    fn id(&self) -> ToolId {
        ToolId::new("boom")
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn required_capabilities(&self) -> &[Capability] {
        if self.gated {
            &[Capability::ShellExec]
        } else {
            &[]
        }
    }
    async fn invoke(
        &self,
        _context: &ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // This is a local signal at the awaited production tool boundary, not
        // a probe-call ordinal or a timing race.
        if self.cancel_in_tool {
            self.cancel.store(true, Ordering::SeqCst);
        }
        if self.time_out {
            std::future::pending::<()>().await;
        }
        if self.succeed {
            return Ok(ToolOutput {
                content: json!({}),
                receipt_data: json!({}),
                cost: CostTuple::cents(2),
            });
        }
        Err(ToolError::ExecutionFailed(
            "controlled local failure".into(),
        ))
    }
}

async fn cancel_failure_case(cancel_in_tool: bool, time_out: bool) {
    let state = support::tempdir().unwrap();
    let session = SessionId::new();
    let receipt_path = state.path().join("receipts.jsonl");
    let journal_root = state.path().join("journals");
    let journal = Arc::new(FileSessionJournal::new(&journal_root, session).unwrap());
    let flag = Arc::new(AtomicBool::new(false));
    let effect_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(CancelThenFail {
            cancel: flag.clone(),
            cancel_in_tool,
            time_out,
            succeed: false,
            gated: false,
            calls: effect_calls.clone(),
            schema: ToolSchema {
                description: "local cancellation/failure boundary".into(),
                input_schema: json!({"type":"object"}),
                output_schema: json!({"type":"object"}),
                examples: vec![],
            },
        }))
        .unwrap();
    let provider = Arc::new(PaidToolRequest::new("boom"));
    let runtime = runtime_builder(provider.clone())
        .with_tools(Arc::new(registry))
        .with_journal(journal.clone())
        .receipt_log(&receipt_path)
        .tool_timeout(std::time::Duration::from_millis(10))
        .projected_envelope(CostEnvelope {
            cents_max: 10,
            ..Default::default()
        })
        .provision_budget(gate_holder(), GateCostTuple::cents(10))
        .build()
        .unwrap();
    let probe: CancelProbe = Arc::new(move || flag.load(Ordering::SeqCst));
    let result = runtime
        .submit_with_cancellation(
            request_for("local", &valid_token(), session),
            Default::default(),
            probe,
            None,
        )
        .await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        effect_calls.load(Ordering::SeqCst),
        1,
        "tool body actually reached"
    );
    let after = runtime
        .remaining_budget(&gate_holder())
        .await
        .unwrap()
        .cents;
    let cancelled = matches!(&result, Err(RuntimeError::TurnCancelled));
    if cancel_in_tool {
        assert_eq!(
            (cancelled, after),
            (true, 10),
            "cancellation during the failing tool must preserve precommit zero-debit economics; result={result:?}"
        );
        let records = journal.replay(session).await.unwrap();
        assert_eq!(
            records
                .iter()
                .filter(|r| matches!(r,
            JournalEntry::OperatorExpense { session_id, provider_cost, .. }
                if *session_id == session && provider_cost.cents == 4))
                .count(),
            1,
            "known operator expense survives once, under the correct session"
        );
    } else {
        let error = result.expect_err("present caller receives the actual tool failure");
        if time_out {
            assert!(matches!(&error, RuntimeError::ToolTimeout { .. }));
        } else {
            assert!(matches!(&error, RuntimeError::Internal(_)));
            assert!(error.to_string().contains("controlled local failure"));
        }
        assert_eq!(
            after, 6,
            "non-cancellation failure retains known provider debit"
        );
        let records = journal.replay(session).await.unwrap();
        assert_eq!(
            records
                .iter()
                .filter(|r| matches!(r,
            JournalEntry::CostFinalized { actual, .. } if actual.cents == 4))
                .count(),
            1
        );
    }
    assert!(
        load_persisted_chain(&receipt_path).unwrap().is_empty(),
        "neither failed nor cancelled tool is a successful completion"
    );
}

#[tokio::test]
async fn cancellation_during_tool_failure_preserves_precommit_caller_economics() {
    cancel_failure_case(true, false).await;
}

#[tokio::test]
async fn present_caller_tool_failure_is_a_paid_refusal_control() {
    cancel_failure_case(false, false).await;
}

#[tokio::test]
async fn cancellation_during_tool_timeout_preserves_precommit_caller_economics() {
    cancel_failure_case(true, true).await;
}

#[tokio::test]
async fn present_caller_tool_timeout_is_a_paid_refusal_control() {
    cancel_failure_case(false, true).await;
}

fn approval_tool(
    flag: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    succeed: bool,
) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(CancelThenFail {
            cancel: flag,
            cancel_in_tool: true,
            time_out: false,
            succeed,
            gated: true,
            calls,
            schema: ToolSchema {
                description: "local approval boundary".into(),
                input_schema: json!({"type":"object"}),
                output_schema: json!({"type":"object"}),
                examples: vec![],
            },
        }))
        .unwrap();
    Arc::new(registry)
}

async fn approval_return_case(cancel_at_return: bool, approved: bool) {
    let state = support::tempdir().unwrap();
    let session = SessionId::new();
    let receipt_path = state.path().join("receipts.jsonl");
    let journal =
        Arc::new(FileSessionJournal::new(state.path().join("journals"), session).unwrap());
    let store = ApprovalStore::new(state.path().join("approvals"));
    if approved {
        let id = seed_approved_card(&store, session);
        assert_eq!(store.read(&id).unwrap().status, ApprovalStatus::Approved);
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(PaidToolRequest::new("boom"));
    let runtime = runtime_builder(provider.clone())
        .with_tools(approval_tool(
            Arc::new(AtomicBool::new(false)),
            calls.clone(),
            false,
        ))
        .with_approvals(store.clone())
        .with_approval_gated_capabilities(["cap.shell_exec".to_string()].into())
        .with_journal(journal.clone())
        .receipt_log(&receipt_path)
        .projected_envelope(CostEnvelope {
            cents_max: 10,
            ..Default::default()
        })
        .provision_budget(gate_holder(), GateCostTuple::cents(10))
        .build()
        .unwrap();
    let probe_store = store.clone();
    // Follow the durable approval boundary, not a probe-call ordinal. The
    // caller abandons when the pending proposal or consumed claim appears.
    let probe: CancelProbe = Arc::new(move || {
        cancel_at_return
            && probe_store.list().unwrap().iter().any(|card| {
                card.status
                    == if approved {
                        ApprovalStatus::Consumed
                    } else {
                        ApprovalStatus::Pending
                    }
            })
    });
    let result = runtime
        .submit_with_cancellation(
            request_for("local", &valid_token(), session),
            Default::default(),
            probe,
            None,
        )
        .await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    if cancel_at_return {
        assert!(
            matches!(result, Err(RuntimeError::TurnCancelled)),
            "{result:?}"
        );
        assert_eq!(
            runtime
                .remaining_budget(&gate_holder())
                .await
                .unwrap()
                .cents,
            10
        );
        let records = journal.replay(session).await.unwrap();
        assert_eq!(records.len(), 1);
        assert!(
            matches!(&records[0], JournalEntry::OperatorExpense { provider_cost, .. } if provider_cost.cents == 4)
        );
    } else {
        assert!(
            matches!(result, Err(RuntimeError::ApprovalRequired { .. })),
            "{result:?}"
        );
        assert_eq!(
            runtime
                .remaining_budget(&gate_holder())
                .await
                .unwrap()
                .cents,
            6
        );
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "an abandoned approval return cannot invoke a tool"
    );
    let cards = store.list().unwrap();
    assert_eq!(cards.len(), 1);
    assert!(
        cards[0].invocation_outcome.is_none(),
        "no returned tool outcome to invent"
    );
    let chain = load_persisted_chain(&receipt_path).unwrap();
    assert!(
        chain
            .iter()
            .all(|r| r.body.verb.as_str() == "approval.propose.created.v1"
                && r.body.cost == CostTuple::ZERO)
    );
}

fn seed_approved_card(store: &ApprovalStore, session: SessionId) -> String {
    let digest = ardur_receipt::Sha256Digest::of(&serde_json::to_vec(&json!({})).unwrap()).to_hex();
    let card = store
        .propose(
            "boom",
            "cap.shell_exec",
            &digest,
            Some(session.0.to_string()),
            "local test",
            support::NOW_UNIX,
        )
        .unwrap();
    let id = card.id.unwrap();
    store
        .decide(&id, Decision::Approve, support::NOW_UNIX)
        .unwrap();
    id
}

#[tokio::test]
async fn cancellation_at_approval_refusal_preserves_precommit_economics() {
    approval_return_case(true, false).await;
}

#[tokio::test]
async fn cancellation_at_approval_success_skips_tool_dispatch() {
    approval_return_case(true, true).await;
}

#[tokio::test]
async fn present_caller_approval_refusal_is_paid() {
    approval_return_case(false, false).await;
}

async fn cancelled_approved_outcome(succeed: bool) {
    let state = support::tempdir().unwrap();
    let session = SessionId::new();
    let store = ApprovalStore::new(state.path().join("approvals"));
    let id = seed_approved_card(&store, session);
    let journal =
        Arc::new(FileSessionJournal::new(state.path().join("journals"), session).unwrap());
    let flag = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = runtime_builder(Arc::new(PaidToolRequest::new("boom")))
        .with_tools(approval_tool(flag.clone(), calls.clone(), succeed))
        .with_approvals(store.clone())
        .with_approval_gated_capabilities(["cap.shell_exec".to_string()].into())
        .with_journal(journal.clone())
        .projected_envelope(CostEnvelope {
            cents_max: 10,
            ..Default::default()
        })
        .provision_budget(gate_holder(), GateCostTuple::cents(10))
        .build()
        .unwrap();
    let result = runtime
        .submit_with_cancellation(
            request_for("local", &valid_token(), session),
            Default::default(),
            Arc::new(move || flag.load(Ordering::SeqCst)),
            None,
        )
        .await;
    assert!(matches!(result, Err(RuntimeError::TurnCancelled)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime
            .remaining_budget(&gate_holder())
            .await
            .unwrap()
            .cents,
        10
    );
    let card = store.read(&id).unwrap();
    assert_eq!(card.status, ApprovalStatus::Consumed);
    assert_eq!(
        card.invocation_outcome.unwrap().result,
        if succeed {
            InvocationResult::Completed
        } else {
            InvocationResult::Failed
        }
    );
    let records = journal.replay(session).await.unwrap();
    assert_eq!(records.len(), 1);
    let known = if succeed { 6 } else { 4 };
    assert!(
        matches!(&records[0], JournalEntry::OperatorExpense { provider_cost, .. } if provider_cost.cents == known)
    );
}

#[tokio::test]
async fn cancellation_retains_verified_failed_approval_outcome() {
    cancelled_approved_outcome(false).await;
}

#[tokio::test]
async fn cancellation_retains_verified_completed_approval_outcome() {
    cancelled_approved_outcome(true).await;
}

struct ParkErrorObserver {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl LifecycleHook for ParkErrorObserver {
    async fn on_error(&self, _ctx: &ErrorCtx<'_>) -> Result<(), HookError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
    fn hook_id(&self) -> HookId {
        HookId::new("park-error-observer")
    }
}

async fn error_observation_case(cancel_in_tool: bool) {
    let state = support::tempdir().unwrap();
    let session = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(state.path(), session).unwrap());
    let flag = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(CancelThenFail {
            cancel: flag.clone(),
            cancel_in_tool,
            time_out: false,
            succeed: false,
            gated: false,
            calls: calls.clone(),
            schema: ToolSchema {
                description: "local observer boundary".into(),
                input_schema: json!({}),
                output_schema: json!({}),
                examples: vec![],
            },
        }))
        .unwrap();
    let hook = Arc::new(ParkErrorObserver {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let mut hooks = HookRegistry::new();
    hooks.register(hook.clone());
    let runtime = runtime_builder(Arc::new(PaidToolRequest::new("boom")))
        .with_tools(Arc::new(tools))
        .registry(Arc::new(hooks))
        .with_journal(journal.clone())
        .projected_envelope(CostEnvelope {
            cents_max: 10,
            ..Default::default()
        })
        .provision_budget(gate_holder(), GateCostTuple::cents(10))
        .build()
        .unwrap();
    let submit = runtime.submit_with_cancellation(
        request_for("local", &valid_token(), session),
        Default::default(),
        Arc::new(move || flag.load(Ordering::SeqCst)),
        None,
    );
    tokio::pin!(submit);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::select! {
            () = hook.entered.notified() => {}
            result = &mut submit => panic!("observer was not reached: {result:?}"),
        }
    })
    .await
    .expect("submit error observer was never entered");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime
            .remaining_budget(&gate_holder())
            .await
            .unwrap()
            .cents,
        if cancel_in_tool { 10 } else { 6 },
        "economics must settle before awaiting the error observer"
    );
    assert_eq!(journal.replay(session).await.unwrap().len(), 1);
    hook.release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), submit)
        .await
        .expect("submit did not finish after observer release");
    if cancel_in_tool {
        assert!(matches!(result, Err(RuntimeError::TurnCancelled)));
    } else {
        assert!(matches!(result, Err(RuntimeError::Internal(_))));
    }
}

#[tokio::test]
async fn paid_refusal_economics_do_not_wait_for_error_observer() {
    error_observation_case(false).await;
}

#[tokio::test]
async fn cancellation_economics_do_not_wait_for_error_observer() {
    error_observation_case(true).await;
}

/// Native stream: usage is priced by the runtime, never a discarded
/// CompletionResponse.cost from the default stream-flattening adapter.
struct NativePaidToolStream {
    tool: &'static str,
    calls: AtomicUsize,
    rate: RateCard,
}

impl NativePaidToolStream {
    fn usage() -> Usage {
        Usage {
            tokens_in: 2,
            tokens_out: 3,
            ..Default::default()
        }
    }
}

#[async_trait]
impl Provider for NativePaidToolStream {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        panic!("the refusal fixture must dispatch the native stream")
    }
    async fn stream(&self, _request: CompletionRequest) -> Result<ProviderStream, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let call = ToolCall {
            id: "native-local-call".into(),
            name: self.tool.into(),
            arguments: json!({}),
        };
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::ToolCallStart(call.clone())),
            Ok(StreamEvent::Usage(Self::usage())),
            Ok(StreamEvent::Finish(FinishReason::ToolUse(vec![call]))),
        ])))
    }
    fn id(&self) -> ProviderId {
        ProviderId("native-local-stream".into())
    }
    fn supports_streaming(&self) -> bool {
        true
    }
    fn rate_card(&self) -> &RateCard {
        &self.rate
    }
}

#[derive(Clone, Copy, Debug)]
enum StreamRefusal {
    UnknownTool,
    ToolAuthorization,
    Capability,
    ApprovalRequired,
    ApprovalRejected,
    ToolError,
    OutputBlock,
    OutputError,
    Timeout,
}

async fn stream_refusal_observer_case(case: StreamRefusal) {
    let state = support::tempdir().unwrap();
    let session = SessionId::new();
    let receipt_path = state.path().join("receipts.jsonl");
    let journal_root = state.path().join("journals");
    let journal = Arc::new(FileSessionJournal::new(&journal_root, session).unwrap());
    let provider = Arc::new(NativePaidToolStream {
        tool: if matches!(case, StreamRefusal::UnknownTool) {
            "unregistered"
        } else {
            "boom"
        },
        calls: AtomicUsize::new(0),
        rate: RateCard {
            version_id: "local-stream-rate".into(),
            cents_per_1k_input: 1000.0,
            cents_per_1k_output: 500.0,
            cents_per_request: 0.0,
        },
    });
    let provider_cost = CostTuple {
        tokens_in: 2,
        tokens_out: 3,
        cents: 4,
        ..Default::default()
    };
    assert_eq!(
        provider.rate.price(NativePaidToolStream::usage()),
        provider_cost
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let completed = matches!(
        case,
        StreamRefusal::OutputBlock | StreamRefusal::OutputError
    );
    let invoked = completed || matches!(case, StreamRefusal::ToolError | StreamRefusal::Timeout);
    let store = ApprovalStore::new(state.path().join("approvals"));
    if invoked {
        seed_approved_card(&store, session);
    } else if matches!(case, StreamRefusal::ApprovalRejected) {
        let digest =
            ardur_receipt::Sha256Digest::of(&serde_json::to_vec(&json!({})).unwrap()).to_hex();
        let card = store
            .propose(
                "boom",
                "cap.shell_exec",
                &digest,
                Some(session.0.to_string()),
                "local test",
                support::NOW_UNIX,
            )
            .unwrap();
        store
            .decide(
                &card.id.unwrap(),
                Decision::Reject {
                    reason: "local rejection".into(),
                },
                support::NOW_UNIX,
            )
            .unwrap();
    }
    let filter = Arc::new(CancelInOutputScan {
        cancel: Arc::new(AtomicBool::new(false)),
        cancel_in_scan: false,
        scan_error: matches!(case, StreamRefusal::OutputError),
        calls: AtomicUsize::new(0),
    });
    let filters = FilterRegistry::new();
    if completed {
        filters.register(filter.clone());
    }
    let hook = Arc::new(ParkErrorObserver {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let mut hooks = HookRegistry::new();
    hooks.register(hook.clone());
    let runtime = runtime_builder(provider.clone())
        .with_tools(counted_tool(
            calls.clone(),
            completed,
            true,
            matches!(case, StreamRefusal::Timeout),
        ))
        .with_approvals(store.clone())
        .with_approval_gated_capabilities(["cap.shell_exec".to_string()].into())
        .with_injection_filters(filters)
        .registry(Arc::new(hooks))
        .with_journal(journal.clone())
        .receipt_log(&receipt_path)
        .tool_timeout(std::time::Duration::from_millis(10))
        .projected_envelope(CostEnvelope {
            tokens_in_max: 1000,
            tokens_out_max: 1000,
            cents_max: 10,
            ..Default::default()
        })
        .provision_budget(
            gate_holder(),
            GateCostTuple {
                tokens_in: 1000,
                tokens_out: 1000,
                cents: 10,
                ..Default::default()
            },
        )
        .build()
        .unwrap();
    let token = match case {
        StreamRefusal::ToolAuthorization => {
            support::mint_token_as(support::HOLDER, support::AUDIENCE, &[support::TOOL])
        }
        StreamRefusal::Capability => {
            support::mint_token_as(support::HOLDER, support::AUDIENCE, &[support::TOOL, "boom"])
        }
        _ => valid_token(),
    };
    let mut stream = Box::pin(runtime.stream(request_for("local", &token, session)));
    let mut events = Vec::new();
    // Poll the retained owning stream THROUGH each yield until on_error is
    // entered and Pending. Dropping a next() borrow does not drop this owner.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            tokio::select! {
                () = hook.entered.notified() => break,
                item = stream.next() => events.push(item.expect("EOF before observer").expect("error before observer")),
            }
        }
    }).await.expect("error observer was never entered");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), usize::from(invoked));
    assert_eq!(filter.calls.load(Ordering::SeqCst), usize::from(completed));
    let balance_while_parked = runtime.remaining_budget(&gate_holder()).await.unwrap();
    // Reopen a real file journal while the observer is still parked. Read both
    // evidence surfaces before checking either, including on the RED run.
    let reopened = FileSessionJournal::new(&journal_root, session).unwrap();
    let records_while_parked = reopened.replay(session).await.unwrap();
    let cards_while_parked = store.list().unwrap();
    hook.release.notify_one();
    let err = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("stream did not resume after observer release")
        .expect("typed refusal item")
        .expect_err("never successful");
    match case {
        StreamRefusal::UnknownTool => assert!(matches!(err, RuntimeError::UnknownTool { .. })),
        StreamRefusal::ToolAuthorization | StreamRefusal::Capability => {
            assert!(matches!(err, RuntimeError::CapDenied { .. }))
        }
        StreamRefusal::ApprovalRequired => {
            assert!(matches!(err, RuntimeError::ApprovalRequired { .. }))
        }
        StreamRefusal::ApprovalRejected => {
            assert!(matches!(err, RuntimeError::ApprovalRejected { .. }))
        }
        StreamRefusal::ToolError => assert!(
            matches!(err, RuntimeError::Internal(ref e) if e.to_string().contains("controlled local failure"))
        ),
        StreamRefusal::OutputBlock => assert!(matches!(err, RuntimeError::InjectionBlocked { .. })),
        StreamRefusal::OutputError => assert!(
            matches!(err, RuntimeError::Internal(ref e) if e.to_string().contains("local scan error"))
        ),
        StreamRefusal::Timeout => assert!(matches!(err, RuntimeError::ToolTimeout { .. })),
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("stream did not terminate after refusal")
            .is_none(),
        "typed error must terminate at EOF"
    );
    use StageKind::*;
    let mut expected = Vec::new();
    for stage in [CapTokenVerify, CedarCheck, InjectionScan, CostGateAdmit] {
        expected.push(FusedEvent::StageStart { stage });
        expected.push(FusedEvent::StageEnd { stage, ok: true });
    }
    expected.extend([
        FusedEvent::StageStart {
            stage: ProviderStream,
        },
        FusedEvent::ToolCallStart {
            id: "native-local-call".into(),
            name: provider.tool.into(),
        },
        FusedEvent::Usage(NativePaidToolStream::usage()),
        FusedEvent::StageEnd {
            stage: ProviderStream,
            ok: true,
        },
        FusedEvent::StageStart { stage: ToolExec },
        FusedEvent::StageEnd {
            stage: ToolExec,
            ok: false,
        },
    ]);
    assert_eq!(
        events, expected,
        "observable stage/result order remains unchanged"
    );
    if invoked {
        assert_eq!(cards_while_parked.len(), 1);
        let card = &cards_while_parked[0];
        assert_eq!(card.status, ApprovalStatus::Consumed);
        if matches!(case, StreamRefusal::Timeout) {
            assert!(
                card.invocation_outcome.is_none(),
                "timeout outcome remains uncertain"
            );
        } else {
            assert_eq!(
                card.invocation_outcome.as_ref().unwrap().result,
                if completed {
                    InvocationResult::Completed
                } else {
                    InvocationResult::Failed
                }
            );
        }
    } else if matches!(
        case,
        StreamRefusal::ApprovalRequired | StreamRefusal::ApprovalRejected
    ) {
        assert_eq!(cards_while_parked.len(), 1);
        assert_eq!(
            cards_while_parked[0].status,
            if matches!(case, StreamRefusal::ApprovalRequired) {
                ApprovalStatus::Pending
            } else {
                ApprovalStatus::Denied
            }
        );
        assert!(cards_while_parked[0].invocation_outcome.is_none());
    } else {
        assert!(cards_while_parked.is_empty());
    }
    let reason = match case {
        StreamRefusal::UnknownTool => "refusal:unknown_tool",
        StreamRefusal::ToolAuthorization => "refusal:tool_authorization",
        StreamRefusal::Capability => "refusal:capability_denied",
        StreamRefusal::ApprovalRequired => "refusal:approval_required",
        StreamRefusal::ApprovalRejected => "refusal:approval_rejected",
        StreamRefusal::ToolError => "refusal:tool_error",
        StreamRefusal::OutputBlock | StreamRefusal::OutputError => "refusal:output_scan",
        StreamRefusal::Timeout => "refusal:tool_timeout_uncertain_effect",
    };
    let known = provider_cost.saturating_add(&CostTuple::cents(if completed { 2 } else { 0 }));
    assert_eq!(
        (balance_while_parked, records_while_parked.len()),
        (
            GateCostTuple {
                tokens_in: 998,
                tokens_out: 997,
                cents: if completed { 4 } else { 6 },
                ..Default::default()
            },
            1
        ),
        "{case:?}: known-cost debit and durable record must precede observer; records={records_while_parked:?}"
    );
    assert!(
        matches!(&records_while_parked[0], JournalEntry::CostFinalized { actual, reason: Some(record_reason), .. }
        if *actual == GateCostTuple { tokens_in: known.tokens_in, tokens_out: known.tokens_out, cents: known.cents, wall_ms: known.wall_ms, attention_score: known.attention_score } && record_reason == reason)
    );
    assert_eq!(
        journal.replay(session).await.unwrap().len(),
        1,
        "observer release must not settle twice"
    );
    let chain = load_persisted_chain(&receipt_path).unwrap();
    assert_eq!(
        chain.len(),
        usize::from(matches!(case, StreamRefusal::ApprovalRequired))
    );
    assert!(
        chain
            .iter()
            .all(|r| r.body.verb.as_str() == "approval.propose.created.v1"
                && r.body.cost == CostTuple::ZERO)
    );
    ardur_fused_runtime::verify_persisted_chain_with_jwks(
        &chain,
        &ardur_receipt::Jwks::from_public_key(&support::receipt_key().public_key()),
    )
    .unwrap();
}

macro_rules! stream_observer_test {
    ($name:ident, $case:ident) => {
        #[tokio::test]
        async fn $name() {
            stream_refusal_observer_case(StreamRefusal::$case).await;
        }
    };
}
stream_observer_test!(stream_observer_unknown_tool, UnknownTool);
stream_observer_test!(stream_observer_tool_authorization, ToolAuthorization);
stream_observer_test!(stream_observer_capability, Capability);
stream_observer_test!(stream_observer_approval_required, ApprovalRequired);
stream_observer_test!(stream_observer_approval_rejected, ApprovalRejected);
stream_observer_test!(stream_observer_tool_error, ToolError);
stream_observer_test!(stream_observer_output_block, OutputBlock);
stream_observer_test!(stream_observer_output_error, OutputError);
// Timeout was already ordered correctly: this is a preservation control.
stream_observer_test!(stream_observer_timeout_preservation, Timeout);

struct CancelInOutputScan {
    cancel: Arc<AtomicBool>,
    cancel_in_scan: bool,
    scan_error: bool,
    calls: AtomicUsize,
}

#[async_trait]
impl InjectionFilter for CancelInOutputScan {
    async fn scan(&self, content: &ScannableContent) -> Result<ScanResult, FilterError> {
        let verdict = if matches!(content, ScannableContent::ToolOutput { .. }) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.cancel_in_scan {
                self.cancel.store(true, Ordering::SeqCst);
            }
            if self.scan_error {
                return Err(FilterError::InvalidInput("local scan error".into()));
            }
            Verdict::Block {
                reason: "local scan refusal".into(),
            }
        } else {
            Verdict::Allow
        };
        Ok(ScanResult {
            verdict,
            flags: vec![],
            confidence: 1.0,
            scan_duration_ms: 0,
        })
    }
    fn filter_id(&self) -> FilterId {
        FilterId::new("cancel-in-output-scan")
    }
    fn confidence_threshold(&self) -> f32 {
        0.5
    }
}

async fn output_scan_case(cancel_in_scan: bool, scan_error: bool) {
    let state = support::tempdir().unwrap();
    let session = SessionId::new();
    let receipt_path = state.path().join("receipts.jsonl");
    let journal =
        Arc::new(FileSessionJournal::new(state.path().join("journals"), session).unwrap());
    let flag = Arc::new(AtomicBool::new(false));
    let filter = Arc::new(CancelInOutputScan {
        cancel: flag.clone(),
        cancel_in_scan,
        scan_error,
        calls: AtomicUsize::new(0),
    });
    let filters = FilterRegistry::new();
    filters.register(filter.clone());
    let provider = Arc::new(PaidToolRequest::new("boom"));
    let runtime = runtime_builder(provider.clone())
        .with_tools(support::paid_registry("boom", CostTuple::cents(2)))
        .with_injection_filters(filters)
        .with_journal(journal.clone())
        .receipt_log(&receipt_path)
        .projected_envelope(CostEnvelope {
            cents_max: 10,
            ..Default::default()
        })
        .provision_budget(gate_holder(), GateCostTuple::cents(10))
        .build()
        .unwrap();
    let result = runtime
        .submit_with_cancellation(
            request_for("local", &valid_token(), session),
            Default::default(),
            Arc::new(move || flag.load(Ordering::SeqCst)),
            None,
        )
        .await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        filter.calls.load(Ordering::SeqCst),
        1,
        "completed tool output was scanned"
    );
    let balance = runtime
        .remaining_budget(&gate_holder())
        .await
        .unwrap()
        .cents;
    let records = journal.replay(session).await.unwrap();
    if cancel_in_scan {
        assert!(
            matches!(result, Err(RuntimeError::TurnCancelled)),
            "{result:?}"
        );
        assert_eq!(balance, 10);
        assert_eq!(records.len(), 1);
        assert!(
            matches!(&records[0], JournalEntry::OperatorExpense { provider_cost, .. } if provider_cost.cents == 6),
            "both the known provider and completed tool cost survive"
        );
    } else {
        if scan_error {
            assert!(matches!(result, Err(RuntimeError::Internal(_))));
        } else {
            assert!(matches!(result, Err(RuntimeError::InjectionBlocked { .. })));
        }
        assert_eq!(balance, 4);
        assert_eq!(records.len(), 1);
        assert!(
            matches!(&records[0], JournalEntry::CostFinalized { actual, .. } if actual.cents == 6)
        );
    }
    assert!(load_persisted_chain(&receipt_path).unwrap().is_empty());
}

#[tokio::test]
async fn cancellation_during_output_scan_refusal_preserves_completed_tool_cost() {
    output_scan_case(true, false).await;
}

#[tokio::test]
async fn cancellation_during_output_scan_error_preserves_completed_tool_cost() {
    output_scan_case(true, true).await;
}

#[tokio::test]
async fn present_caller_output_scan_refusal_is_paid() {
    output_scan_case(false, false).await;
}

#[tokio::test]
async fn present_caller_output_scan_error_is_paid() {
    output_scan_case(false, true).await;
}

/// Models a supported async journal whose write acknowledgement is lost AFTER
/// a real durable append, contrasted with an error before any write. This does
/// not pretend to be a kernel fsync injection; it is the trait-boundary failure.
struct AcknowledgementFailureJournal {
    real: Arc<FileSessionJournal>,
    after_write: bool,
    append_calls: AtomicUsize,
}

#[async_trait]
impl SessionJournal for AcknowledgementFailureJournal {
    async fn append_settlement(
        &self,
        _id: uuid::Uuid,
        entry: JournalEntry,
    ) -> ardur_session_journals::ProjectionOutcome {
        // The fixture can prove its BEFORE-write arm does not enter the backend.
        // Generic Io/replay absence alone never supplies this proof.
        let result = self.append(entry).await;
        match result {
            Ok(id) => ardur_session_journals::ProjectionOutcome::Durable(id),
            Err(e) if !self.after_write => {
                ardur_session_journals::ProjectionOutcome::DefinitelyNotApplied(e)
            }
            Err(e) => ardur_session_journals::ProjectionOutcome::Unknown(e),
        }
    }
    async fn append(&self, entry: JournalEntry) -> Result<EntryId, JournalError> {
        self.append_calls.fetch_add(1, Ordering::SeqCst);
        if self.after_write {
            self.real.append(entry).await?;
        }
        Err(JournalError::Io(std::io::Error::other(
            "controlled journal acknowledgement failure",
        )))
    }
    async fn replay(&self, session: SessionId) -> Result<Vec<JournalEntry>, JournalError> {
        self.real.replay(session).await
    }
    async fn replay_from(
        &self,
        session: SessionId,
        from: EntryId,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        self.real.replay_from(session, from).await
    }
    async fn close(&self) -> Result<(), JournalError> {
        self.real.close().await
    }
    fn session_id(&self) -> &SessionId {
        self.real.session_id()
    }
}

/// Fail only settlement appends, so an approval proposal's unrelated evidence
/// cannot steal the refusal being tested. Snapshot callbacks AFTER the failed
/// append returns, not just before entering the journal implementation.
struct ObservedSettlementFailureJournal {
    failure: AcknowledgementFailureJournal,
    observer: Arc<RecordingHook>,
    failed_attempts: std::sync::Mutex<Vec<(JournalEntry, Vec<HookEvent>)>>,
}

#[async_trait]
impl SessionJournal for ObservedSettlementFailureJournal {
    async fn append_settlement(
        &self,
        _id: uuid::Uuid,
        entry: JournalEntry,
    ) -> ardur_session_journals::ProjectionOutcome {
        let result = self.append(entry).await;
        match result {
            Ok(id) => ardur_session_journals::ProjectionOutcome::Durable(id),
            Err(e) if !self.failure.after_write => {
                ardur_session_journals::ProjectionOutcome::DefinitelyNotApplied(e)
            }
            Err(e) => ardur_session_journals::ProjectionOutcome::Unknown(e),
        }
    }
    async fn append(&self, entry: JournalEntry) -> Result<EntryId, JournalError> {
        if matches!(entry, JournalEntry::CostFinalized { .. }) {
            let result = self.failure.append(entry.clone()).await;
            assert!(result.is_err(), "the real fault boundary must fail");
            self.failed_attempts
                .lock()
                .unwrap()
                .push((entry, self.observer.events()));
            result
        } else {
            self.failure.real.append(entry).await
        }
    }
    async fn replay(&self, session: SessionId) -> Result<Vec<JournalEntry>, JournalError> {
        self.failure.replay(session).await
    }
    async fn replay_from(
        &self,
        session: SessionId,
        from: EntryId,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        self.failure.replay_from(session, from).await
    }
    async fn close(&self) -> Result<(), JournalError> {
        self.failure.close().await
    }
    fn session_id(&self) -> &SessionId {
        self.failure.session_id()
    }
}

#[tokio::test]
async fn submit_timeout_failed_settlement_notifies_original_observer() {
    let state = support::tempdir().unwrap();
    let session = SessionId::new();
    let receipt_path = state.path().join("receipts.jsonl");
    let journal_root = state.path().join("journals");
    let observer = Arc::new(RecordingHook::new(HookId::new(
        "submit-timeout-failed-settlement",
    )));
    let journal = Arc::new(ObservedSettlementFailureJournal {
        failure: AcknowledgementFailureJournal {
            real: Arc::new(FileSessionJournal::new(&journal_root, session).unwrap()),
            after_write: false,
            append_calls: AtomicUsize::new(0),
        },
        observer: observer.clone(),
        failed_attempts: Default::default(),
    });
    let known = CostTuple {
        tokens_in: 2,
        tokens_out: 3,
        cents: 4,
        wall_ms: 5,
        attention_score: 6,
    };
    let provider = Arc::new(PaidToolRequest {
        cost: known,
        ..PaidToolRequest::new("boom")
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let store = ApprovalStore::new(state.path().join("approvals"));
    let approval = seed_approved_card(&store, session);
    let mut hooks = HookRegistry::new();
    hooks.register(observer.clone());
    let budget = GateCostTuple {
        tokens_in: 10,
        tokens_out: 10,
        cents: 10,
        wall_ms: 10,
        attention_score: 10,
    };
    let runtime = runtime_builder(provider.clone())
        .with_tools(counted_tool(calls.clone(), false, true, true))
        .with_approvals(store.clone())
        .with_approval_gated_capabilities(["cap.shell_exec".to_string()].into())
        .registry(Arc::new(hooks))
        .with_journal(journal.clone())
        .receipt_log(&receipt_path)
        .tool_timeout(std::time::Duration::from_millis(10))
        .projected_envelope(CostEnvelope {
            tokens_in_max: 10,
            tokens_out_max: 10,
            cents_max: 10,
            wall_ms_max: 10,
            attention_score_max: 10,
        })
        .provision_budget(gate_holder(), budget)
        .build()
        .unwrap();
    // Drive the actual paid completion and pending tool to a terminal return.
    // This outer bound must fail, never substitute for the per-tool timeout.
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        runtime.submit(request_for("local", &valid_token(), session)),
    )
    .await
    .expect("submit timeout settlement did not finish")
    .expect_err("settlement must fail");
    let errors_at_terminal: Vec<_> = observer
        .events()
        .into_iter()
        .filter(|event| matches!(event, HookEvent::OnError { .. }))
        .collect();
    let RuntimeError::Internal(ref detail) = error else {
        panic!("settlement failure must remain primary, got {error:?}");
    };
    assert_eq!(
        detail.to_string(),
        "settlement journal projection definitely not applied"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "pending tool was entered");
    assert_eq!(journal.failure.append_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime.remaining_budget(&gate_holder()).await.unwrap(),
        budget,
        "fail-before-write retains the current full-refund policy on every axis"
    );
    assert!(
        FileSessionJournal::new(&journal_root, session)
            .unwrap()
            .replay(session)
            .await
            .unwrap()
            .is_empty(),
        "no charged record or invented completion may survive"
    );
    assert!(load_persisted_chain(&receipt_path).unwrap().is_empty());
    let cards = store.list().unwrap();
    assert_eq!(cards.len(), 1);
    let card = store.read(&approval).unwrap();
    assert_eq!(card.status, ApprovalStatus::Consumed);
    assert!(
        card.invocation_outcome.is_none(),
        "timeout is not verified failure"
    );
    {
        let attempts = journal.failed_attempts.lock().unwrap();
        assert_eq!(attempts.len(), 1);
        assert!(
            matches!(&attempts[0].0, JournalEntry::CostFinalized { actual, reason: Some(reason), .. }
                if *actual == known && reason == "refusal:tool_timeout_uncertain_effect"),
            "only the exact known provider tuple is settled; pending tool cost is unknown"
        );
        assert!(
            !attempts[0]
                .1
                .iter()
                .any(|event| matches!(event, HookEvent::OnError { .. })),
            "original timeout notification must follow the failed append return"
        );
    }
    assert_eq!(
        errors_at_terminal.len(),
        1,
        "original submit timeout observer must fire exactly once even when settlement fails"
    );
    assert_eq!(
        errors_at_terminal,
        vec![HookEvent::OnError {
            hook_id: observer.hook_id(),
            session_id: session,
            phase: LifecyclePhase::Provider,
            message: RuntimeError::ToolTimeout {
                tool: "boom".into()
            }
            .to_string(),
        }]
    );
}

// Preservation controls share the paid submit path but retain opposite caller
// economics. Park only after the recording hook so durable state is observable
// while the real error callback is still pending.
async fn submit_timeout_observer_control(cancel_in_tool: bool) {
    let state = support::tempdir().unwrap();
    let session = SessionId::new();
    let receipt_path = state.path().join("receipts.jsonl");
    let journal_root = state.path().join("journals");
    let real = Arc::new(FileSessionJournal::new(&journal_root, session).unwrap());
    let observer = Arc::new(RecordingHook::new(HookId::new("submit-timeout-control")));
    let failure = Arc::new(ObservedSettlementFailureJournal {
        failure: AcknowledgementFailureJournal {
            real: real.clone(),
            after_write: false,
            append_calls: AtomicUsize::new(0),
        },
        observer: observer.clone(),
        failed_attempts: Default::default(),
    });
    let journal: Arc<dyn SessionJournal> = if cancel_in_tool {
        // A cancellation must bypass charged settlement, even with its fault armed.
        failure.clone()
    } else {
        real
    };
    let park = Arc::new(ParkErrorObserver {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let mut hooks = HookRegistry::new();
    hooks.register(observer.clone());
    hooks.register(park.clone());
    let known = CostTuple {
        tokens_in: 2,
        tokens_out: 3,
        cents: 4,
        wall_ms: 5,
        attention_score: 6,
    };
    let provider = Arc::new(PaidToolRequest {
        cost: known,
        ..PaidToolRequest::new("boom")
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let store = ApprovalStore::new(state.path().join("approvals"));
    let approval = seed_approved_card(&store, session);
    let budget = GateCostTuple {
        tokens_in: 10,
        tokens_out: 10,
        cents: 10,
        wall_ms: 10,
        attention_score: 10,
    };
    let expected_balance = if cancel_in_tool {
        budget
    } else {
        GateCostTuple {
            tokens_in: budget.tokens_in - known.tokens_in,
            tokens_out: budget.tokens_out - known.tokens_out,
            cents: budget.cents - known.cents,
            wall_ms: budget.wall_ms - known.wall_ms,
            attention_score: budget.attention_score - known.attention_score,
        }
    };
    let runtime = runtime_builder(provider.clone())
        .with_tools(counted_tool(calls.clone(), false, true, true))
        .with_approvals(store.clone())
        .with_approval_gated_capabilities(["cap.shell_exec".to_string()].into())
        .registry(Arc::new(hooks))
        .with_journal(journal)
        .receipt_log(&receipt_path)
        .tool_timeout(std::time::Duration::from_millis(10))
        .projected_envelope(CostEnvelope {
            tokens_in_max: 10,
            tokens_out_max: 10,
            cents_max: 10,
            wall_ms_max: 10,
            attention_score_max: 10,
        })
        .provision_budget(gate_holder(), budget)
        .build()
        .unwrap();
    let invoked = calls.clone();
    let submit = runtime.submit_with_cancellation(
        request_for("local", &valid_token(), session),
        Default::default(),
        // Signal only from the actual tool entry, not from probe-call ordinals.
        Arc::new(move || cancel_in_tool && invoked.load(Ordering::SeqCst) > 0),
        None,
    );
    tokio::pin!(submit);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::select! {
            () = park.entered.notified() => {}
            result = &mut submit => panic!("timeout observer was not reached: {result:?}"),
        }
    })
    .await
    .expect("submit timeout observer was never entered");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime.remaining_budget(&gate_holder()).await.unwrap(),
        expected_balance,
        "all economics must be applied before the observer can park"
    );
    let records = FileSessionJournal::new(&journal_root, session)
        .unwrap()
        .replay(session)
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    if cancel_in_tool {
        assert!(
            matches!(&records[0], JournalEntry::OperatorExpense {
                session_id, provider_cost, class, ..
            } if *session_id == session && *provider_cost == known && class == "cancelled_precommit"),
            "cancellation preserves the whole known tuple, never pending tool cost"
        );
    } else {
        assert!(
            matches!(&records[0], JournalEntry::CostFinalized { actual, reason: Some(reason), .. }
                if *actual == known && reason == "refusal:tool_timeout_uncertain_effect"),
            "healthy settlement must be durable before timeout observation"
        );
    }
    assert_eq!(failure.failure.append_calls.load(Ordering::SeqCst), 0);
    assert!(failure.failed_attempts.lock().unwrap().is_empty());
    assert!(load_persisted_chain(&receipt_path).unwrap().is_empty());
    assert_eq!(store.list().unwrap().len(), 1);
    let card = store.read(&approval).unwrap();
    assert_eq!(card.status, ApprovalStatus::Consumed);
    assert!(
        card.invocation_outcome.is_none(),
        "timeout outcome stays unknown"
    );
    let original = if cancel_in_tool {
        RuntimeError::TurnCancelled
    } else {
        RuntimeError::ToolTimeout {
            tool: "boom".into(),
        }
    };
    let expected_errors = vec![HookEvent::OnError {
        hook_id: observer.hook_id(),
        session_id: session,
        phase: LifecyclePhase::Provider,
        message: original.to_string(),
    }];
    let errors = || {
        observer
            .events()
            .into_iter()
            .filter(|event| matches!(event, HookEvent::OnError { .. }))
            .collect::<Vec<_>>()
    };
    assert_eq!(errors(), expected_errors);
    park.release.notify_one();
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), submit)
        .await
        .expect("submit did not finish after timeout observer release")
        .expect_err("timeout/cancellation cannot be a successful completion");
    if cancel_in_tool {
        assert!(matches!(error, RuntimeError::TurnCancelled), "{error:?}");
    } else {
        assert!(
            matches!(&error, RuntimeError::ToolTimeout { tool } if tool == "boom"),
            "{error:?}"
        );
    }
    assert_eq!(
        errors(),
        expected_errors,
        "no duplicate notification at return"
    );
    assert_eq!(
        runtime.remaining_budget(&gate_holder()).await.unwrap(),
        expected_balance
    );
    let terminal_records = FileSessionJournal::new(&journal_root, session)
        .unwrap()
        .replay(session)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(terminal_records).unwrap(),
        serde_json::to_value(records).unwrap()
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(store.read(&approval).unwrap().invocation_outcome.is_none());
    assert!(load_persisted_chain(&receipt_path).unwrap().is_empty());
}

#[tokio::test]
async fn submit_timeout_healthy_settlement_precedes_original_observer() {
    submit_timeout_observer_control(false).await;
}

#[tokio::test]
async fn submit_timeout_cancellation_precedes_failed_settlement_observer() {
    submit_timeout_observer_control(true).await;
}

async fn stream_failed_settlement_observer_case(case: StreamRefusal) {
    let state = support::tempdir().unwrap();
    let session = SessionId::new();
    let receipt_path = state.path().join("receipts.jsonl");
    let journal_root = state.path().join("journals");
    let observer = Arc::new(RecordingHook::new(HookId::new(
        "failed-settlement-observer",
    )));
    let journal = Arc::new(ObservedSettlementFailureJournal {
        failure: AcknowledgementFailureJournal {
            real: Arc::new(FileSessionJournal::new(&journal_root, session).unwrap()),
            after_write: false,
            append_calls: AtomicUsize::new(0),
        },
        observer: observer.clone(),
        failed_attempts: Default::default(),
    });
    let provider = Arc::new(NativePaidToolStream {
        tool: if matches!(case, StreamRefusal::UnknownTool) {
            "unregistered"
        } else {
            "boom"
        },
        calls: AtomicUsize::new(0),
        rate: RateCard {
            version_id: "local-stream-rate".into(),
            cents_per_1k_input: 1000.0,
            cents_per_1k_output: 500.0,
            cents_per_request: 0.0,
        },
    });
    let known = CostTuple {
        tokens_in: 2,
        tokens_out: 3,
        cents: 4,
        ..Default::default()
    };
    assert_eq!(provider.rate.price(NativePaidToolStream::usage()), known);
    let calls = Arc::new(AtomicUsize::new(0));
    let completed = matches!(
        case,
        StreamRefusal::OutputBlock | StreamRefusal::OutputError
    );
    let invoked = completed || matches!(case, StreamRefusal::ToolError | StreamRefusal::Timeout);
    let store = ApprovalStore::new(state.path().join("approvals"));
    if invoked {
        seed_approved_card(&store, session);
    } else if matches!(case, StreamRefusal::ApprovalRejected) {
        let digest =
            ardur_receipt::Sha256Digest::of(&serde_json::to_vec(&json!({})).unwrap()).to_hex();
        let card = store
            .propose(
                "boom",
                "cap.shell_exec",
                &digest,
                Some(session.0.to_string()),
                "local test",
                support::NOW_UNIX,
            )
            .unwrap();
        store
            .decide(
                &card.id.unwrap(),
                Decision::Reject {
                    reason: "local rejection".into(),
                },
                support::NOW_UNIX,
            )
            .unwrap();
    }
    let filter = Arc::new(CancelInOutputScan {
        cancel: Arc::new(AtomicBool::new(false)),
        cancel_in_scan: false,
        scan_error: matches!(case, StreamRefusal::OutputError),
        calls: AtomicUsize::new(0),
    });
    let filters = FilterRegistry::new();
    if completed {
        filters.register(filter.clone());
    }
    let known = known.saturating_add(&CostTuple::cents(if completed { 2 } else { 0 }));
    let mut hooks = HookRegistry::new();
    hooks.register(observer.clone());
    let budget = GateCostTuple {
        tokens_in: 1000,
        tokens_out: 1000,
        cents: 10,
        ..Default::default()
    };
    let runtime = runtime_builder(provider.clone())
        .with_tools(counted_tool(
            calls.clone(),
            completed,
            true,
            matches!(case, StreamRefusal::Timeout),
        ))
        .with_approvals(store.clone())
        .with_approval_gated_capabilities(["cap.shell_exec".to_string()].into())
        .with_injection_filters(filters)
        .registry(Arc::new(hooks))
        .with_journal(journal.clone())
        .receipt_log(&receipt_path)
        .tool_timeout(std::time::Duration::from_millis(10))
        .projected_envelope(CostEnvelope {
            tokens_in_max: 1000,
            tokens_out_max: 1000,
            cents_max: 10,
            ..Default::default()
        })
        .provision_budget(gate_holder(), budget)
        .build()
        .unwrap();
    let token = match case {
        StreamRefusal::ToolAuthorization => {
            support::mint_token_as(support::HOLDER, support::AUDIENCE, &[support::TOOL])
        }
        StreamRefusal::Capability => {
            support::mint_token_as(support::HOLDER, support::AUDIENCE, &[support::TOOL, "boom"])
        }
        _ => valid_token(),
    };
    let mut stream = Box::pin(runtime.stream(request_for("local", &token, session)));
    // Own and fully drain through the terminal settlement error AND EOF.
    // A dropped next() or parked owner is not evidence for this regression.
    let (mut items, errors_at_terminal) =
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut items = Vec::new();
            let mut errors_at_terminal = Vec::new();
            while let Some(item) = stream.next().await {
                if item.is_err() {
                    errors_at_terminal = observer
                        .events()
                        .into_iter()
                        .filter(|event| matches!(event, HookEvent::OnError { .. }))
                        .collect();
                }
                items.push(item);
            }
            (items, errors_at_terminal)
        })
        .await
        .expect("failed-settlement stream did not reach EOF");
    let error = items
        .pop()
        .expect("terminal item")
        .expect_err("settlement must fail");
    let RuntimeError::Internal(ref detail) = error else {
        panic!("expected Internal settlement error, got {error:?}");
    };
    assert_eq!(
        detail.to_string(),
        "settlement journal projection definitely not applied"
    );
    let events: Vec<_> = items
        .into_iter()
        .map(|item| item.expect("only terminal item may fail"))
        .collect();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), usize::from(invoked));
    assert_eq!(filter.calls.load(Ordering::SeqCst), usize::from(completed));
    assert_eq!(journal.failure.append_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime.remaining_budget(&gate_holder()).await.unwrap(),
        budget,
        "keep the current fail-before-write full-refund policy"
    );
    assert!(
        FileSessionJournal::new(&journal_root, session)
            .unwrap()
            .replay(session)
            .await
            .unwrap()
            .is_empty()
    );
    let chain = load_persisted_chain(&receipt_path).unwrap();
    assert_eq!(
        chain.len(),
        usize::from(matches!(case, StreamRefusal::ApprovalRequired))
    );
    assert!(
        chain
            .iter()
            .all(|r| r.body.verb.as_str() == "approval.propose.created.v1"
                && r.body.cost == CostTuple::ZERO)
    );
    ardur_fused_runtime::verify_persisted_chain_with_jwks(
        &chain,
        &ardur_receipt::Jwks::from_public_key(&support::receipt_key().public_key()),
    )
    .unwrap();
    let cards = store.list().unwrap();
    if invoked {
        assert_eq!(cards.len(), 1);
        let card = &cards[0];
        assert_eq!(card.status, ApprovalStatus::Consumed);
        if matches!(case, StreamRefusal::Timeout) {
            assert!(
                card.invocation_outcome.is_none(),
                "timeout outcome remains uncertain"
            );
        } else {
            assert_eq!(
                card.invocation_outcome.as_ref().unwrap().result,
                if completed {
                    InvocationResult::Completed
                } else {
                    InvocationResult::Failed
                }
            );
        }
    } else if matches!(
        case,
        StreamRefusal::ApprovalRequired | StreamRefusal::ApprovalRejected
    ) {
        assert_eq!(cards.len(), 1);
        assert_eq!(
            cards[0].status,
            if matches!(case, StreamRefusal::ApprovalRequired) {
                ApprovalStatus::Pending
            } else {
                ApprovalStatus::Denied
            }
        );
        assert!(cards[0].invocation_outcome.is_none());
    } else {
        assert!(cards.is_empty());
    }
    let reason = match case {
        StreamRefusal::UnknownTool => "refusal:unknown_tool",
        StreamRefusal::ToolAuthorization => "refusal:tool_authorization",
        StreamRefusal::Capability => "refusal:capability_denied",
        StreamRefusal::ApprovalRequired => "refusal:approval_required",
        StreamRefusal::ApprovalRejected => "refusal:approval_rejected",
        StreamRefusal::ToolError => "refusal:tool_error",
        StreamRefusal::OutputBlock | StreamRefusal::OutputError => "refusal:output_scan",
        StreamRefusal::Timeout => "refusal:tool_timeout_uncertain_effect",
    };
    {
        let attempts = journal.failed_attempts.lock().unwrap();
        assert_eq!(attempts.len(), 1);
        assert!(
            matches!(&attempts[0].0, JournalEntry::CostFinalized { actual, reason: Some(record_reason), .. }
            if *actual == known && record_reason == reason)
        );
        assert!(
            !attempts[0]
                .1
                .iter()
                .any(|event| matches!(event, HookEvent::OnError { .. })),
            "refusal notification must follow the settlement attempt"
        );
    }
    use StageKind::*;
    let mut expected = Vec::new();
    for stage in [CapTokenVerify, CedarCheck, InjectionScan, CostGateAdmit] {
        expected.push(FusedEvent::StageStart { stage });
        expected.push(FusedEvent::StageEnd { stage, ok: true });
    }
    expected.extend([
        FusedEvent::StageStart {
            stage: ProviderStream,
        },
        FusedEvent::ToolCallStart {
            id: "native-local-call".into(),
            name: provider.tool.into(),
        },
        FusedEvent::Usage(NativePaidToolStream::usage()),
        FusedEvent::StageEnd {
            stage: ProviderStream,
            ok: true,
        },
        FusedEvent::StageStart { stage: ToolExec },
        FusedEvent::StageEnd {
            stage: ToolExec,
            ok: false,
        },
    ]);
    let errors: Vec<_> = observer
        .events()
        .into_iter()
        .filter(|event| matches!(event, HookEvent::OnError { .. }))
        .collect();
    assert_eq!(
        errors.len(),
        1,
        "original refusal observer must fire exactly once even when settlement fails"
    );
    assert_eq!(
        errors_at_terminal, errors,
        "the original observer must finish before the terminal error, with no later duplicate at EOF"
    );
    let original = match case {
        StreamRefusal::UnknownTool => RuntimeError::UnknownTool { tool: provider.tool.into() },
        StreamRefusal::ToolAuthorization => RuntimeError::CapDenied { reason: "tool not in cap-token allowlist".into() },
        StreamRefusal::Capability => RuntimeError::CapDenied { reason: "tool `boom` requires capability `cap.shell_exec` which is not granted by the cap-token".into() },
        StreamRefusal::ApprovalRequired => RuntimeError::ApprovalRequired {
            tool: "boom".into(), approval_id: cards[0].id.clone().unwrap(),
            reason: "tool `boom` requires capability `cap.shell_exec`, which is approval-gated".into(),
        },
        StreamRefusal::ApprovalRejected => RuntimeError::ApprovalRejected {
            tool: "boom".into(), approval_id: cards[0].id.clone().unwrap(), reason: "local rejection".into(),
        },
        StreamRefusal::ToolError => RuntimeError::Internal(anyhow::anyhow!("tool `boom` failed: {}", ToolError::ExecutionFailed("controlled local failure".into()))),
        StreamRefusal::OutputBlock => RuntimeError::injection_blocked("injection-defense", "local scan refusal", vec![]),
        StreamRefusal::OutputError => RuntimeError::Internal(anyhow::anyhow!("tool-output injection scan failed: {}", FilterError::InvalidInput("local scan error".into()))),
        StreamRefusal::Timeout => RuntimeError::ToolTimeout { tool: "boom".into() },
    };
    let phase = match case {
        StreamRefusal::UnknownTool | StreamRefusal::ToolError | StreamRefusal::Timeout => {
            LifecyclePhase::Provider
        }
        _ => LifecyclePhase::Submit,
    };
    assert_eq!(
        errors,
        vec![HookEvent::OnError {
            hook_id: observer.hook_id(),
            session_id: session,
            phase,
            message: original.to_string(),
        }]
    );
    assert_eq!(
        events, expected,
        "exact stage order before the terminal error"
    );
}

macro_rules! stream_failed_settlement_test {
    ($name:ident, $case:ident) => {
        #[tokio::test]
        async fn $name() {
            stream_failed_settlement_observer_case(StreamRefusal::$case).await;
        }
    };
}
stream_failed_settlement_test!(stream_failed_settlement_observer_unknown_tool, UnknownTool);
stream_failed_settlement_test!(
    stream_failed_settlement_observer_tool_authorization,
    ToolAuthorization
);
stream_failed_settlement_test!(stream_failed_settlement_observer_capability, Capability);
stream_failed_settlement_test!(
    stream_failed_settlement_observer_approval_required,
    ApprovalRequired
);
stream_failed_settlement_test!(
    stream_failed_settlement_observer_approval_rejected,
    ApprovalRejected
);
stream_failed_settlement_test!(stream_failed_settlement_observer_tool_error, ToolError);
stream_failed_settlement_test!(stream_failed_settlement_observer_output_block, OutputBlock);
stream_failed_settlement_test!(stream_failed_settlement_observer_output_error, OutputError);
// Adjacent preexisting failed-settlement gap, independently reproduced.
stream_failed_settlement_test!(stream_failed_settlement_observer_timeout, Timeout);

async fn journal_failure_case(after_write: bool) {
    let state = support::tempdir().unwrap();
    let session = SessionId::new();
    let root = state.path().join("journals");
    let real = Arc::new(FileSessionJournal::new(&root, session).unwrap());
    let journal = Arc::new(AcknowledgementFailureJournal {
        real: real.clone(),
        after_write,
        append_calls: AtomicUsize::new(0),
    });
    let provider = Arc::new(PaidToolRequest::new("unregistered.local.tool"));
    let runtime = runtime_builder(provider.clone())
        .with_journal(journal.clone())
        .projected_envelope(CostEnvelope {
            cents_max: 10,
            ..Default::default()
        })
        .provision_budget(gate_holder(), GateCostTuple::cents(10))
        .build()
        .unwrap();
    let error = runtime
        .submit(request_for("local", &valid_token(), session))
        .await
        .expect_err("journal failure is surfaced");
    assert!(error.to_string().contains("settlement"), "{error}");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(journal.append_calls.load(Ordering::SeqCst), 1);
    let supervisor = runtime.settlement_supervisor();
    let snapshots = supervisor.durable_snapshots().unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].turn().rounds[0].known_incurred.cents, 4);
    if after_write {
        assert!(matches!(
            supervisor.status().turns[0].error,
            Some(ardur_fused_runtime::settlement::SettlementError::ProjectionUnknown)
        ));
        assert!(runtime.drain_pending_settlements().await.is_err());
        assert_eq!(
            journal.append_calls.load(Ordering::SeqCst),
            1,
            "ambiguity is not blindly retried"
        );
    }
    let balance = runtime
        .remaining_budget(&gate_holder())
        .await
        .unwrap()
        .cents;
    drop(runtime);
    drop(journal);
    drop(real);
    let reopened = FileSessionJournal::new(&root, session).unwrap();
    let entries = reopened.replay(session).await.unwrap();
    assert!(
        !entries
            .iter()
            .any(|r| matches!(r, JournalEntry::AssistantMessage { .. }))
    );
    if after_write {
        assert_eq!(
            entries.len(),
            1,
            "real append is durable despite lost acknowledgement"
        );
        let JournalEntry::CostFinalized { actual, .. } = &entries[0] else {
            panic!("fixture did not persist its charged record");
        };
        assert_eq!(actual.cents, 4);
        assert_eq!(
            10 - balance,
            actual.cents,
            "unconditionally rolling back an ambiguous append leaves durable finalized cost without its debit or a compensating/unresolved record"
        );
    } else {
        assert!(
            entries.is_empty(),
            "fail-before-write contrast truly wrote nothing"
        );
        assert_eq!(
            balance, 10,
            "fail-before-write restores this live-process debit"
        );
    }
}

#[tokio::test]
async fn ambiguous_journal_ack_must_not_leave_unqualified_charge_after_refund() {
    journal_failure_case(true).await;
}

#[tokio::test]
async fn journal_failure_before_write_is_the_refund_control() {
    journal_failure_case(false).await;
}
