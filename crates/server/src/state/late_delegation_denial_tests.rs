//! Late child denials must survive the real tool/runtime boundary and remain
//! visible to the server's security consumers. Only the provider is scripted.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ardur_cap_token::{
    BiscuitCapTokenIssuer, CapScope, CapToken, CapTokenIssuer, HolderId, KeyPair, PublicKey,
};
use ardur_cedar_policy::{CedarPolicyBundle, PolicyBundle, PolicySource};
use ardur_cost_gate::{
    CostEnvelope, CostTuple as GateCostTuple, HolderId as GateHolderId, ManualClock, UnixTsMillis,
};
use ardur_delegate_tool::DelegateTaskTool;
use ardur_fused_runtime::{FusedEvent, FusedRuntime, FusedRuntimeBuilder, SharedDenyList};
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, ModelId, Provider, ProviderError,
    ProviderStream, RateCard, StreamEvent, Usage,
};
use ardur_receipt::Es256SigningKey;
use ardur_runtime::{
    CapTokenRef, ChatMessage, ChatRuntime, CostTuple, ProviderId, Role, RuntimeError, SessionId,
    SubmitRequest, ToolCall,
};
use ardur_tool_registry::{
    Capability, Tool, ToolContext, ToolError, ToolId, ToolOutput, ToolRegistry, ToolSchema,
};
use async_trait::async_trait;
use futures::StreamExt as _;
use serde_json::json;

use super::SecurityMetrics;
use crate::security_events::{GATE_CAP_TOKEN, SecurityEvent, SecurityEventLog};

const AUDIENCE: &str = "late-delegation-test";
const HOLDER: &str = "late-delegation-parent";
const NOW_MS: u64 = 1_750_000_000_000;

#[derive(Default)]
struct Probe {
    revoke_next: AtomicBool,
    entered: AtomicUsize,
    completed: AtomicUsize,
}

/// The registry sees the REAL delegate's id, schema and nonempty capabilities.
/// Reaching invoke therefore proves the final outer capability gate passed.
/// The only intervention is revocation before forwarding to the real worker,
/// attenuation, child verifier and termination path; no fake denial is returned.
struct RevokeAtInvoke {
    delegate: DelegateTaskTool<SharedDenyList>,
    root: PublicKey,
    deny: SharedDenyList,
    probe: Arc<Probe>,
}

#[async_trait]
impl Tool for RevokeAtInvoke {
    fn id(&self) -> ToolId {
        self.delegate.id()
    }

    fn schema(&self) -> &ToolSchema {
        self.delegate.schema()
    }

    fn required_capabilities(&self) -> &[Capability] {
        self.delegate.required_capabilities()
    }

    async fn invoke(
        &self,
        ctx: &ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        self.probe.entered.fetch_add(1, Ordering::SeqCst);
        if self.probe.revoke_next.swap(false, Ordering::SeqCst) {
            let token = CapToken::from_base64(&ctx.cap_token.0, &self.root).expect("valid parent");
            self.deny
                .revoke_token(&token)
                .expect("revocation acknowledged");
        }
        let output = self.delegate.invoke(ctx, args).await?;
        assert_eq!(output.content["outcome"], "completed");
        self.probe.completed.fetch_add(1, Ordering::SeqCst);
        Ok(output)
    }
}

struct DelegatingProvider {
    complete_calls: AtomicUsize,
    stream_calls: AtomicUsize,
    rate_card: RateCard,
}

impl DelegatingProvider {
    fn response(req: &CompletionRequest) -> CompletionResponse {
        let finish_reason = if req
            .messages
            .iter()
            .any(|message| message.role == Role::Tool)
        {
            FinishReason::Stop
        } else {
            FinishReason::ToolUse(vec![ToolCall {
                id: "delegate-call".to_string(),
                name: DelegateTaskTool::ID.to_string(),
                arguments: json!({"goal": "local child probe", "max_cost_cents": 1}),
            }])
        };
        CompletionResponse {
            content: String::new(),
            finish_reason,
            usage: Usage::default(),
            cost: CostTuple::default(),
            raw_provider_response: None,
        }
    }
}

#[async_trait]
impl Provider for DelegatingProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.complete_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Self::response(&req))
    }

    async fn stream(&self, req: CompletionRequest) -> Result<ProviderStream, ProviderError> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        let response = Self::response(&req);
        Ok(Box::pin(futures::stream::iter([
            Ok(StreamEvent::Usage(response.usage)),
            Ok(StreamEvent::Finish(response.finish_reason)),
        ])))
    }

    fn id(&self) -> ProviderId {
        ProviderId("local-delegation-probe".to_string())
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

fn mint(issuer: &BiscuitCapTokenIssuer) -> String {
    issuer
        .issue(
            HolderId(HOLDER.to_string()),
            CapScope {
                audience: AUDIENCE.to_string(),
                // The real child uses wall time; keep expiry far in the future.
                expires_unix: 4_000_000_000,
                budget_remaining: 1_000_000,
                tool_allowlist: vec![
                    "chat.submit".to_string(),
                    "memory.write".to_string(),
                    DelegateTaskTool::ID.to_string(),
                    "cap.multi_agent_delegate".to_string(),
                ],
            },
        )
        .expect("issue token")
        .to_base64()
        .expect("encode token")
}

async fn turn(runtime: &FusedRuntime, token: &str, streaming: bool) -> Result<(), RuntimeError> {
    let req = SubmitRequest {
        messages: vec![ChatMessage::user("delegate a local task")],
        cap_token: CapTokenRef(token.to_string()),
        session_id: SessionId::new(),
        requested_provider: None,
    };
    if !streaming {
        return runtime.submit(req).await.map(|_| ());
    }
    let events: Vec<_> = runtime.stream(req).collect().await;
    if events.iter().any(Result::is_err) {
        assert_eq!(events.iter().filter(|event| event.is_err()).count(), 1);
        assert!(events.last().expect("terminal event").is_err());
        assert!(
            !events.iter().any(|event| matches!(
                event,
                Ok(FusedEvent::ToolCallResult { .. }
                    | FusedEvent::Receipt { .. }
                    | FusedEvent::Finish(_))
            )),
            "a denied child must not produce a successful tool result or parent receipt"
        );
    } else {
        assert!(matches!(
            events.last(),
            Some(Ok(FusedEvent::Finish(FinishReason::Stop)))
        ));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Ok(FusedEvent::ToolCallResult { .. })))
        );
    }
    for event in events {
        event?;
    }
    Ok(())
}

async fn late_denial(streaming: bool) -> RuntimeError {
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let root = issuer.public_key();
    let token = mint(&issuer);
    let control = mint(&issuer);
    let deny = SharedDenyList::new();
    let probe = Arc::new(Probe::default());
    let delegate = DelegateTaskTool::with_deny_list(root, AUDIENCE, deny.clone());
    assert!(!delegate.required_capabilities().is_empty());
    let mut tools = ToolRegistry::new();
    tools
        .register(Box::new(RevokeAtInvoke {
            delegate,
            root,
            deny: deny.clone(),
            probe: probe.clone(),
        }))
        .expect("register real delegate wrapper");
    let provider = Arc::new(DelegatingProvider {
        complete_calls: AtomicUsize::new(0),
        stream_calls: AtomicUsize::new(0),
        rate_card: RateCard::anthropic_2026_q2_v1(),
    });
    let policy = CedarPolicyBundle::load(PolicySource::Embedded(
        "permit(principal, action, resource);".to_string(),
    ))
    .expect("test policy");
    let storage = tempfile::tempdir().expect("owned settlement fixture");
    let receipt_log = storage
        .path()
        .canonicalize()
        .unwrap()
        .join("receipts.jsonl");
    let runtime = FusedRuntimeBuilder::new(
        root,
        policy,
        provider.clone(),
        Es256SigningKey::generate(),
        ModelId::new("local-test"),
    )
    .audience(AUDIENCE)
    .tool("chat.submit")
    .clock(Arc::new(ManualClock::new(UnixTsMillis(NOW_MS))))
    .projected_envelope(CostEnvelope {
        tokens_in_max: 1_000,
        tokens_out_max: 1_000,
        cents_max: 100,
        wall_ms_max: 1_000,
        attention_score_max: 1_000,
    })
    .provision_budget(
        GateHolderId(HOLDER.to_string()),
        GateCostTuple {
            tokens_in: 1_000_000,
            tokens_out: 1_000_000,
            cents: 1_000_000,
            wall_ms: 1_000_000,
            attention_score: 1_000_000,
        },
    )
    .deny_list(deny)
    .with_tools(Arc::new(tools))
    .receipt_log(receipt_log)
    .build()
    .expect("fused runtime");

    turn(&runtime, &token, streaming)
        .await
        .expect("same authority delegates before revocation");
    probe.revoke_next.store(true, Ordering::SeqCst);
    let err = turn(&runtime, &token, streaming)
        .await
        .expect_err("late revoked child must fail");
    assert_eq!(
        probe.entered.load(Ordering::SeqCst),
        2,
        "final outer gate must pass before revocation"
    );
    assert!(
        !probe.revoke_next.load(Ordering::SeqCst),
        "invoke must execute the revocation seam"
    );
    assert_eq!(
        probe.completed.load(Ordering::SeqCst),
        1,
        "revoked child must not complete"
    );
    turn(&runtime, &control, streaming)
        .await
        .expect("unrelated authority still delegates");
    assert_eq!(probe.entered.load(Ordering::SeqCst), 3);
    assert_eq!(probe.completed.load(Ordering::SeqCst), 2);
    assert_eq!(
        provider.complete_calls.load(Ordering::SeqCst),
        if streaming { 0 } else { 5 }
    );
    assert_eq!(
        provider.stream_calls.load(Ordering::SeqCst),
        if streaming { 5 } else { 0 }
    );
    err
}

fn assert_server_denial(err: &RuntimeError) {
    let metrics = SecurityMetrics::default();
    metrics.record_err(err);
    let snapshot = metrics.snapshot();
    assert_eq!(
        snapshot.cap_denied, 1,
        "late child denial must increment cap_denied"
    );
    assert_eq!(
        snapshot.other_errors, 0,
        "late child denial must not increment other_errors"
    );
    assert_eq!(snapshot.turns_ok, 0);
    let event =
        SecurityEvent::from_error(err, NOW_MS).expect("late child denial must be auditable");
    assert_eq!(event.gate, GATE_CAP_TOKEN);
    assert_eq!(event.decision, "deny");
    let dir = tempfile::tempdir().expect("audit fixture");
    let log = SecurityEventLog::in_data_dir(dir.path());
    log.record_denial(err, NOW_MS);
    let text = std::fs::read_to_string(log.path()).expect("denial audit persisted");
    let records: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("audit JSON"))
        .collect();
    assert_eq!(
        records,
        vec![serde_json::to_value(event).expect("expected audit event")]
    );
}

#[tokio::test]
async fn submit_preserves_late_real_delegate_denial() {
    let err = late_denial(false).await;
    assert!(
        matches!(err, RuntimeError::CapDenied { .. }),
        "submit must preserve late child authorization denial, got {err:?}"
    );
    assert_server_denial(&err);
}

#[tokio::test]
async fn stream_preserves_late_real_delegate_denial() {
    let err = late_denial(true).await;
    assert!(
        matches!(err, RuntimeError::CapDenied { .. }),
        "stream must preserve late child authorization denial, got {err:?}"
    );
    assert_server_denial(&err);
}
