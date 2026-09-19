//! Exercise the real Processor with overlapping FusedRuntime calls, without a worker pool.
use super::*;
use ardur_lifecycle_hooks::{HookError, HookId, HookRegistry, LifecycleHook, PostReceiptCtx};
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, ProviderError, RateCard, Usage,
};
use ardur_runtime::{CostTuple, ProviderId, Role, ToolCall};
use ardur_tool_registry::{
    Capability, Tool, ToolContext, ToolError, ToolId, ToolOutput, ToolSchema,
};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Notify;

struct NamedTool {
    name: &'static str,
    schema: ToolSchema,
}
#[async_trait]
impl Tool for NamedTool {
    fn id(&self) -> ToolId {
        ToolId::new(self.name)
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn required_capabilities(&self) -> &[Capability] {
        &[]
    }
    async fn invoke(&self, _: &ToolContext, _: serde_json::Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            content: json!(self.name),
            receipt_data: json!(null),
            cost: CostTuple {
                cents: 3,
                ..Default::default()
            },
        })
    }
}
struct ToolProvider {
    rates: RateCard,
}
#[async_trait]
impl Provider for ToolProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let name = req
            .messages
            .iter()
            .find(|m| m.role == Role::User)
            .unwrap()
            .content
            .clone();
        let finished = req.messages.iter().any(|m| m.role == Role::Tool);
        let cents = if name == "alpha" { 7 } else { 19 };
        Ok(CompletionResponse {
            content: name.clone(),
            finish_reason: if finished {
                FinishReason::Stop
            } else {
                FinishReason::ToolUse(vec![ToolCall {
                    id: "same-call-id".into(),
                    name,
                    arguments: json!({}),
                }])
            },
            usage: Usage::default(),
            cost: CostTuple {
                tokens_in: cents,
                tokens_out: 2,
                cents,
                ..Default::default()
            },
            raw_provider_response: None,
        })
    }
    fn id(&self) -> ProviderId {
        ProviderId("attribution".into())
    }
    fn supports_streaming(&self) -> bool {
        false
    }
    fn rate_card(&self) -> &RateCard {
        &self.rates
    }
}
struct ParkFinal {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}
#[async_trait]
impl LifecycleHook for ParkFinal {
    fn hook_id(&self) -> HookId {
        HookId::new("park-final-attribution")
    }
    async fn on_post_receipt(&self, ctx: &PostReceiptCtx<'_>) -> Result<(), HookError> {
        if ctx.response.content == "alpha"
            && matches!(ctx.response.finish_reason, FinishReason::Stop)
        {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(())
    }
}

async fn overlapping_turns(same_session: bool) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let key = Es256SigningKey::generate();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut hooks = HookRegistry::new();
    hooks.register(Arc::new(ParkFinal {
        entered: entered.clone(),
        release: release.clone(),
    }));
    let mut tools = ToolRegistry::new();
    for name in ["alpha", "beta"] {
        tools
            .register(Box::new(NamedTool {
                name,
                schema: ToolSchema {
                    description: "local attribution fixture".into(),
                    input_schema: json!({"type":"object"}),
                    output_schema: json!({"type":"string"}),
                    examples: vec![],
                },
            }))
            .unwrap();
    }
    let receipt_log = root.join("chain.jsonl");
    let runtime = FusedRuntimeBuilder::new(
        issuer.public_key(),
        CedarPolicyBundle::load(PolicySource::Embedded(
            "permit(principal, action, resource);".into(),
        ))
        .unwrap(),
        Arc::new(ToolProvider {
            rates: RateCard::anthropic_2026_q2_v1(),
        }),
        key.clone(),
        ModelId::new("fixture"),
    )
    .audience(AUDIENCE)
    .tool(TOOL)
    .provision_budget(GateHolderId(GATEWAY_SUBJECT.into()), gateway_budget(10_000))
    .projected_envelope(per_turn_envelope(10_000))
    .with_tools(Arc::new(tools))
    .registry(Arc::new(hooks))
    .receipt_log(&receipt_log)
    .build()
    .unwrap();
    let processor = Processor {
        runtime,
        slack: None,
        matrix: Arc::new(OnceLock::new()),
        discord: Arc::new(OnceLock::new()),
        telegram: Arc::new(OnceLock::new()),
        issuer,
        cap_budget_remaining: 10_000,
        tool_allowlist: vec![TOOL.into(), "alpha".into(), "beta".into()],
        receipt_log: receipt_log.clone(),
        receipt_jwks: ardur_receipt::Jwks::from_public_key(&key.public_key()),
        receipt_cache: Arc::new(VerifiedReceiptCache::new()),
        security_metrics: Arc::new(SecurityMetrics::default()),
        security_events: SecurityEventLog::in_data_dir(&root),
    };
    let first_session = SessionId::new();
    let second_session = if same_session {
        first_session
    } else {
        SessionId::new()
    };
    let (first_tx, first_rx) = oneshot::channel();
    let (second_tx, second_rx) = oneshot::channel();
    let turn = |message: &str, session_id, reply| HttpTurn {
        message: message.into(),
        session_id,
        reply,
        caller_gone: Arc::new(AtomicBool::new(false)),
        handshake: TurnCommitHandshake::new(),
    };
    let first = async {
        processor
            .handle_http(turn("alpha", first_session, first_tx))
            .await;
        first_rx.await.unwrap().unwrap()
    };
    let second = async {
        entered.notified().await; // first is committed, but handle_http has not correlated it yet
        processor
            .handle_http(turn("beta", second_session, second_tx))
            .await;
        let result = second_rx.await.unwrap().unwrap();
        release.notify_one();
        result
    };
    let (first, second) = tokio::time::timeout(Duration::from_secs(10), async {
        futures::join!(first, second)
    })
    .await
    .unwrap();
    assert_eq!(
        first.tools_called,
        vec!["alpha"],
        "first turn must not claim later turn's tools"
    );
    assert_eq!(second.tools_called, vec!["beta"]);
    assert_eq!(
        (first.tokens_in, first.tokens_out, first.cents),
        (14, 4, 17)
    );
    assert_eq!(
        (second.tokens_in, second.tokens_out, second.cents),
        (38, 4, 41)
    );
    assert_ne!(first.receipt_id, second.receipt_id);
    let chain = ardur_fused_runtime::load_persisted_chain(&receipt_log).unwrap();
    ardur_fused_runtime::verify_persisted_chain_with_jwks(&chain, &processor.receipt_jwks).unwrap();
    assert_eq!(chain.len(), 4);
    for outcome in [&first, &second] {
        let snapshots = processor
            .runtime
            .settlement_supervisor()
            .durable_snapshots()
            .unwrap();
        let snapshot = snapshots.iter().find(|s| matches!(s.turn().terminal,
            ardur_session_journals::settlement::TurnTerminal::FinalAnswer(id) if id.0.to_string() == outcome.receipt_id)).unwrap();
        assert_eq!(snapshot.turn().request_session, outcome.session_id);
        assert_eq!(snapshot.turn().rounds.len(), 2);
        let final_receipt = chain
            .iter()
            .find(|r| r.body.receipt_id.to_string() == outcome.receipt_id)
            .unwrap();
        assert!(final_receipt.body.tool_calls.is_empty());
        assert!(
            final_receipt.body.cost.cents < outcome.cents,
            "final receipt stays round-local"
        );
    }
}
#[tokio::test]
async fn same_session_overlap_is_turn_local() {
    overlapping_turns(true).await;
}
#[tokio::test]
async fn distinct_session_overlap_is_turn_local() {
    overlapping_turns(false).await;
}
