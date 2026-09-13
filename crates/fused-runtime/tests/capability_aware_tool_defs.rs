//! **gh#415.** What the model is *told* exists must match what it is *allowed*
//! to call.
//!
//! The runtime authorises a tool call at invocation time against the turn's
//! cap-token, but the tool list sent to the provider is built from the whole
//! registry. A turn holding a narrow token is still shown every tool the
//! deployment carries.
//!
//! That is a disclosure problem before it is a usability one. Tool names and
//! descriptions are not neutral: `dolthub.execute`, `obsidian.write` or a
//! customer-named connector tell the model — and anything that can influence or
//! read the transcript — what this deployment is wired to. A capability the
//! operator deliberately withheld should not be discoverable by reading the
//! tool list.
//!
//! The secondary effect is wasted context and avoidable failure: the model
//! spends tokens on definitions it cannot use, and its most plausible next
//! action may be a call that is certain to be denied.

mod support;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ardur_cedar_policy::{CedarPolicyBundle, PolicyBundle, PolicySource};
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, Provider, ProviderError, RateCard, Usage,
};
use ardur_runtime::{ChatRuntime, CostTuple, ProviderId, RuntimeError, ToolCall};
use ardur_tool_registry::{
    Capability, Tool, ToolContext, ToolError, ToolId, ToolOutput, ToolRegistry, ToolSchema,
};
use async_trait::async_trait;
use serde_json::{Value, json};
use support::*;
use tokio::sync::Mutex;

/// A provider that records the tool list it was offered and then stops.
struct RecordingProvider {
    offered: Mutex<Vec<Vec<String>>>,
    rate_card: RateCard,
    calls: AtomicUsize,
}

impl RecordingProvider {
    fn new() -> Self {
        Self {
            offered: Mutex::new(Vec::new()),
            rate_card: RateCard::anthropic_2026_q2_v1(),
            calls: AtomicUsize::new(0),
        }
    }

    /// The tool names offered on the most recent provider call.
    async fn last_offered(&self) -> Vec<String> {
        self.offered
            .lock()
            .await
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait]
impl Provider for RecordingProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.offered
            .lock()
            .await
            .push(req.tools.iter().map(|t| t.name.clone()).collect());
        Ok(CompletionResponse {
            content: "ok".to_string(),
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
            cost: CostTuple::default(),
            raw_provider_response: None,
        })
    }

    fn id(&self) -> ProviderId {
        ProviderId("recording".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// A provider that asks for `tool_name` once, then stops.
struct WantsToolProvider {
    tool_name: String,
    invocations: Arc<AtomicUsize>,
    last_seen: AtomicUsize,
    rate_card: RateCard,
}

impl WantsToolProvider {
    fn new(tool_name: &str, invocations: Arc<AtomicUsize>) -> Self {
        Self {
            tool_name: tool_name.to_string(),
            invocations,
            last_seen: AtomicUsize::new(0),
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }
}

#[async_trait]
impl Provider for WantsToolProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let current = self.invocations.load(Ordering::SeqCst);
        if current > self.last_seen.load(Ordering::SeqCst) {
            self.last_seen.store(current, Ordering::SeqCst);
            return Ok(CompletionResponse {
                content: String::new(),
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
                cost: CostTuple::default(),
                raw_provider_response: None,
            });
        }
        Ok(CompletionResponse {
            content: String::new(),
            finish_reason: FinishReason::ToolUse(vec![ToolCall {
                id: "call_1".to_string(),
                name: self.tool_name.clone(),
                arguments: json!({}),
            }]),
            usage: Usage::default(),
            cost: CostTuple::default(),
            raw_provider_response: None,
        })
    }

    fn id(&self) -> ProviderId {
        ProviderId("wants-tool".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// A tool that declares a capability and records whether it ran.
struct CapTool {
    id: String,
    caps: Vec<Capability>,
    schema: ToolSchema,
    invocations: Arc<AtomicUsize>,
}

impl CapTool {
    fn new(id: &str, caps: Vec<Capability>, invocations: Arc<AtomicUsize>) -> Self {
        Self {
            id: id.to_string(),
            caps,
            schema: ToolSchema {
                description: format!("{id} test tool"),
                input_schema: json!({ "type": "object" }),
                output_schema: json!({ "type": "object" }),
                examples: vec![],
            },
            invocations,
        }
    }
}

#[async_trait]
impl Tool for CapTool {
    fn id(&self) -> ToolId {
        ToolId::new(&self.id)
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn invoke(&self, _ctx: &ToolContext, _args: Value) -> Result<ToolOutput, ToolError> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput {
            content: json!({ "ok": true }),
            cost: CostTuple::default(),
            receipt_data: json!({ "ok": true }),
        })
    }

    fn required_capabilities(&self) -> &[Capability] {
        &self.caps
    }
}

/// One tool needing no capability, one whose very existence is sensitive.
fn two_tool_registry(invocations: Arc<AtomicUsize>) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(CapTool::new("echo", vec![], invocations.clone())))
        .expect("echo id is unique");
    registry
        .register(Box::new(CapTool::new(
            "dolthub.execute",
            vec![Capability::Custom("integration.dolthub.write".to_string())],
            invocations,
        )))
        .expect("dolthub id is unique");
    Arc::new(registry)
}

/// Permits the chat verb but forbids `Action::ToolInvoke`.
fn submit_only_policy() -> CedarPolicyBundle {
    CedarPolicyBundle::load(PolicySource::Embedded(
        "permit(principal, action == Action::\"Submit\", resource);".to_string(),
    ))
    .expect("submit-only policy compiles")
}

/// A registry where EVERY tool needs a capability, so a bare token permits none.
fn only_capability_gated_registry() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry
        .register(Box::new(CapTool::new(
            "dolthub.execute",
            vec![Capability::Custom("integration.dolthub.write".to_string())],
            Arc::new(AtomicUsize::new(0)),
        )))
        .expect("dolthub id is unique");
    Arc::new(registry)
}

/// A tool the turn's cap-token cannot use must not be advertised to the model.
#[tokio::test]
async fn a_tool_the_token_cannot_call_is_not_advertised() {
    let provider = Arc::new(RecordingProvider::new());
    let runtime = runtime_builder(provider.clone())
        .with_tools(two_tool_registry(Arc::new(AtomicUsize::new(0))))
        .build()
        .expect("runtime builds");

    // Scoped to `echo`: no `cap.integration.dolthub.write`.
    let token = mint_token_as(HOLDER, AUDIENCE, &[TOOL, "echo"]);
    runtime
        .submit(user_request("hi", &token))
        .await
        .expect("the turn completes");

    let offered = provider.last_offered().await;
    assert!(
        offered.contains(&"echo".to_string()),
        "a tool the token allows must still be offered: {offered:?}"
    );
    assert!(
        !offered.contains(&"dolthub.execute".to_string()),
        "a tool the token cannot call must not be advertised — its name alone \
         discloses what this deployment is wired to: {offered:?}"
    );
}

/// The filter must not empty the list for a token that grants everything.
///
/// A filter that removed every tool would satisfy the assertion above while
/// breaking the runtime, so the permissive direction is pinned too.
#[tokio::test]
async fn a_token_granting_the_capability_still_sees_the_tool() {
    let provider = Arc::new(RecordingProvider::new());
    let runtime = runtime_builder(provider.clone())
        .with_tools(two_tool_registry(Arc::new(AtomicUsize::new(0))))
        .build()
        .expect("runtime builds");

    let token = mint_token_as(
        HOLDER,
        AUDIENCE,
        &[
            TOOL,
            "echo",
            "dolthub.execute",
            "cap.integration.dolthub.write",
        ],
    );
    runtime
        .submit(user_request("hi", &token))
        .await
        .expect("the turn completes");

    let offered: HashSet<String> = provider.last_offered().await.into_iter().collect();
    assert!(
        offered.contains("echo") && offered.contains("dolthub.execute"),
        "a token granting both capabilities must be offered both: {offered:?}"
    );
}

/// Filtering the advertisement must not become the only thing enforcing it.
///
/// A provider that names an unadvertised tool anyway — scripted here, a
/// confused or manipulated model in production — must still be denied at
/// invocation, and the tool body must not run.
#[tokio::test]
async fn hiding_a_tool_does_not_replace_the_invocation_check() {
    let invocations = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(WantsToolProvider::new(
        "dolthub.execute",
        invocations.clone(),
    ));
    let runtime = runtime_builder(provider)
        .with_tools(two_tool_registry(invocations.clone()))
        .build()
        .expect("runtime builds");

    let token = mint_token_as(HOLDER, AUDIENCE, &[TOOL, "echo"]);
    let result = runtime.submit(user_request("go", &token)).await;

    match result {
        Err(RuntimeError::CapDenied { .. }) => {}
        other => panic!(
            "calling an unadvertised tool must still be denied by the cap check, \
             got {other:?}"
        ),
    }
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        0,
        "the tool body must never run"
    );
}

/// A token that permits nothing yields an empty tool list, not a failed turn.
///
/// Review item 4, checked rather than assumed. An empty list is the same shape
/// a tool-less deployment sends, so the turn proceeds as a plain completion.
/// Failing the turn instead would make a narrow token indistinguishable from a
/// broken one, and would let the *absence* of a capability become an error the
/// model can observe — the same disclosure this change closes.
#[tokio::test]
async fn a_token_permitting_nothing_yields_an_empty_tool_list_not_an_error() {
    let provider = Arc::new(RecordingProvider::new());
    let runtime = runtime_builder(provider.clone())
        .with_tools(only_capability_gated_registry())
        .build()
        .expect("runtime builds");

    // Grants the chat verb but no tool capability at all.
    let token = mint_token_as(HOLDER, AUDIENCE, &[TOOL]);
    runtime
        .submit(user_request("hi", &token))
        .await
        .expect("the turn still completes");

    assert!(
        provider.last_offered().await.is_empty(),
        "no tool is permitted, so none may be advertised"
    );
}

/// A tool Cedar would deny is not advertised either.
///
/// Review item 1, and it found a real gap: the invocation path applies TWO
/// gates — `authorize_tool_invocation` (name-scoped token check plus the Cedar
/// `Action::ToolInvoke` decision) and `authorize_tool_capabilities` (the
/// tool's declared capabilities). Filtering on only the second left a
/// Cedar-denied tool advertised, which is the same disclosure one gate over.
#[tokio::test]
async fn a_tool_cedar_denies_is_not_advertised() {
    let provider = Arc::new(RecordingProvider::new());
    // Permits the chat verb but forbids Action::ToolInvoke, so the turn
    // completes and the advertised list is observable. (deny_all forbids chat
    // too, so the turn errors before any tool list is built — which is how the
    // first version of this test passed while asserting nothing.)
    let runtime = runtime_builder_with_policy(provider.clone(), submit_only_policy())
        .with_tools(two_tool_registry(Arc::new(AtomicUsize::new(0))))
        .build()
        .expect("runtime builds");

    // A token that grants everything, so only Cedar can deny.
    let token = mint_token_as(
        HOLDER,
        AUDIENCE,
        &[
            TOOL,
            "echo",
            "dolthub.execute",
            "cap.integration.dolthub.write",
        ],
    );
    let result = runtime.submit(user_request("hi", &token)).await;

    // The turn may complete (no tool was needed) or be denied by policy; what
    // matters is that nothing Cedar refuses was ever offered.
    // The assertion must actually run: a test whose only check sits behind an
    // `if` that is false proves nothing. Cedar denies ToolInvoke but not the
    // chat verb, so the turn completes and the tool list is observable.
    result.expect("the turn completes; only tool invocation is denied");
    let offered = provider.last_offered().await;
    assert!(
        offered.is_empty(),
        "Cedar denies every tool invoke, so none may be advertised: {offered:?}"
    );
}

/// A capability-FREE tool whose name the token omits is not advertised.
///
/// The precise case review named, and the one my capability-gated tests did not
/// reach: `authorize_tool_capabilities` returns `Ok(())` immediately for a tool
/// declaring no capabilities, so a filter built only on that check advertises
/// the tool even when the token's allowlist omits its ID. The name-scoped check
/// inside `authorize_tool_invocation` is what catches it.
#[tokio::test]
async fn a_capability_free_tool_the_token_omits_is_not_advertised() {
    let provider = Arc::new(RecordingProvider::new());
    let runtime = runtime_builder(provider.clone())
        .with_tools(two_tool_registry(Arc::new(AtomicUsize::new(0))))
        .build()
        .expect("runtime builds");

    // `echo` declares NO capabilities and is deliberately absent from the
    // allowlist; `dolthub.execute` is absent too.
    let token = mint_token_as(HOLDER, AUDIENCE, &[TOOL]);
    runtime
        .submit(user_request("hi", &token))
        .await
        .expect("the turn completes");

    let offered = provider.last_offered().await;
    assert!(
        !offered.contains(&"echo".to_string()),
        "a capability-free tool the token does not name must not be advertised: \
         {offered:?}"
    );
}

/// The token-name check admits a capability-free tool it DOES name.
///
/// The restrictive direction for the test above: a filter that dropped every
/// capability-free tool would satisfy it while hiding legitimately callable
/// tools.
#[tokio::test]
async fn a_capability_free_tool_the_token_names_is_still_advertised() {
    let provider = Arc::new(RecordingProvider::new());
    let runtime = runtime_builder(provider.clone())
        .with_tools(two_tool_registry(Arc::new(AtomicUsize::new(0))))
        .build()
        .expect("runtime builds");

    let token = mint_token_as(HOLDER, AUDIENCE, &[TOOL, "echo"]);
    runtime
        .submit(user_request("hi", &token))
        .await
        .expect("the turn completes");

    let offered = provider.last_offered().await;
    assert!(
        offered.contains(&"echo".to_string()),
        "a capability-free tool the token names must still be offered: {offered:?}"
    );
}
