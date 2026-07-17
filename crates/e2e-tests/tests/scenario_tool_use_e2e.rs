//! §6.0 end-to-end — a full tool-use turn through the fused runtime.
//!
//! A scripted provider asks for the `echo` tool, then (once it sees the result)
//! produces a final answer. The runtime drives the tool-call loop end to end:
//! it invokes the registered tool, folds the result back, re-dispatches, and
//! settles. We assert the public surface — the answer — plus the audit trail the
//! loop leaves behind:
//!
//! - the receipt **chain** has one receipt per provider call, linked and
//!   verifiable off disk;
//! - the tool round's receipt **records the tool call** (name + id);
//! - the turn's **cost accumulates** across both provider calls.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use ardur_cost_gate::CostEnvelope;
use ardur_e2e_tests::fixtures;
use ardur_fused_runtime::{load_persisted_chain, verify_persisted_chain};
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, Provider, ProviderError, RateCard, Usage,
};
use ardur_receipt::Sha256Digest;
use ardur_runtime::{
    CapTokenRef, ChatMessage, ChatRuntime, CostTuple, ProviderId, SessionId, SubmitRequest,
    ToolCall,
};
use ardur_tool_registry::{EchoTool, ToolRegistry};
use async_trait::async_trait;

/// Cents the scripted provider bills per call, so the turn's accumulated cost is
/// a known multiple of the number of provider iterations.
const CENTS_PER_CALL: u64 = 3;

/// A provider that returns a scripted queue of responses (one per call) and
/// bills [`CENTS_PER_CALL`] each time, so cost accumulation across the tool loop
/// is observable.
struct ScriptedBillingProvider {
    responses: Mutex<Vec<CompletionResponse>>,
    calls: Arc<AtomicUsize>,
    rate_card: RateCard,
}

impl ScriptedBillingProvider {
    fn new(mut responses: Vec<CompletionResponse>) -> Self {
        // Pop from the front; reverse so `Vec::pop` yields them in order.
        responses.reverse();
        Self {
            responses: Mutex::new(responses),
            calls: Arc::new(AtomicUsize::new(0)),
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for ScriptedBillingProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut response = self
            .responses
            .lock()
            .expect("scripted provider lock")
            .pop()
            .expect("the scripted provider was called more times than scripted");
        response.cost = CostTuple {
            cents: CENTS_PER_CALL,
            ..CostTuple::default()
        };
        Ok(response)
    }

    fn id(&self) -> ProviderId {
        ProviderId("scripted-billing".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

#[tokio::test]
async fn tool_use_full_turn_audits_and_accumulates_cost() {
    let root = fixtures::temp_session_root();
    let receipt_log = root.path().join("chain.jsonl");

    // The model asks for `echo`, then answers once it has the result.
    let provider = Arc::new(ScriptedBillingProvider::new(vec![
        CompletionResponse {
            content: String::new(),
            finish_reason: FinishReason::ToolUse(vec![ToolCall {
                id: "call_1".to_string(),
                name: "echo".to_string(),
                arguments: serde_json::json!({ "msg": "round-trip me" }),
            }]),
            usage: Usage::default(),
            cost: CostTuple::default(),
            raw_provider_response: None,
        },
        CompletionResponse {
            content: "the tool echoed: round-trip me".to_string(),
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
            cost: CostTuple::default(),
            raw_provider_response: None,
        },
    ]));

    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(EchoTool::new()))
        .expect("echo id is unique");

    // A modest per-iteration envelope (well under the fixture's generous budget)
    // so each round of the tool loop reserves-then-refunds without the second
    // admission tripping the ceiling.
    let runtime = fixtures::fused_builder(provider.clone())
        .with_tools(Arc::new(registry))
        .projected_envelope(CostEnvelope {
            tokens_in_max: 1_000_000,
            tokens_out_max: 1_000_000,
            cents_max: 1_000,
            wall_ms_max: 600_000,
            attention_score_max: 1_000_000,
        })
        .receipt_log(&receipt_log)
        .build()
        .expect("the fused runtime wires with a tool registry");

    let request = SubmitRequest {
        messages: vec![ChatMessage::user("please use the echo tool")],
        cap_token: CapTokenRef(fixtures::dev_valid_cap_token_with_echo()),
        session_id: SessionId::new(),
        requested_provider: None,
    };

    let result = runtime
        .submit(request)
        .await
        .expect("the tool turn completes");

    // ---- public surface: the final answer, after the tool round trip.
    assert_eq!(result.response.content, "the tool echoed: round-trip me");
    assert_eq!(
        provider.call_count(),
        2,
        "one call requested the tool, the second produced the answer"
    );

    // ---- cost accumulates across both provider calls.
    assert_eq!(
        result.cost.cents,
        2 * CENTS_PER_CALL,
        "the turn's cost is the sum of both provider iterations"
    );

    // ---- audit trail: one receipt per provider call, chained + verifiable.
    let chain = load_persisted_chain(&receipt_log).expect("the receipt chain reloads");
    assert_eq!(chain.len(), 2, "one receipt per provider call");
    assert!(
        chain[0].body.parent_hash.is_none(),
        "the first receipt is genesis"
    );
    assert_eq!(
        chain[1].body.parent_hash,
        Some(Sha256Digest::of(chain[0].jws_compact.as_bytes())),
        "the answer receipt chains onto the tool-round receipt"
    );
    verify_persisted_chain(&chain).expect("the full chain verifies off disk");

    // ---- the tool round's receipt records the call it made.
    assert_eq!(
        chain[0].body.tool_calls.len(),
        1,
        "the tool round records exactly one tool call"
    );
    let recorded = &chain[0].body.tool_calls[0];
    assert_eq!(recorded.tool_name, "echo");
    assert_eq!(recorded.call_id, "call_1");
    assert!(
        chain[1].body.tool_calls.is_empty(),
        "the final-answer round records no tool calls"
    );
}
