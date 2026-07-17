//! Shared test scaffolding for the lifecycle-hook integration tests: stub
//! providers (counting / capturing / echoing / erroring) and a handful of
//! purpose-built hooks the per-scenario test files compose.

#![allow(dead_code)] // each test file uses a different subset.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ardur_lifecycle_hooks::{
    HookDecision, HookError, HookId, LifecycleHook, PostReceiptCtx, PreSubmitCtx,
};
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, ModelId, Provider, ProviderError,
    RateCard, Usage,
};
use ardur_runtime::{CapTokenRef, ChatMessage, CostTuple, Role, SessionId, SubmitRequest};
use async_trait::async_trait;
use parking_lot::Mutex;

/// The model id the tests dispatch under.
pub fn test_model() -> ModelId {
    ModelId::new("test-model-v1")
}

/// Build a `SubmitRequest` from a single user message + cap-token string.
pub fn user_request(content: &str, cap_token: &str) -> SubmitRequest {
    SubmitRequest {
        messages: vec![ChatMessage::user(content)],
        cap_token: CapTokenRef(cap_token.to_string()),
        session_id: SessionId::new(),
        requested_provider: None,
    }
}

/// A provider that echoes the last user message back, while recording every
/// request it received and counting its calls. Covers the "was the provider
/// called?" and "what request did it see?" assertions.
pub struct EchoProvider {
    calls: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<CompletionRequest>>>,
    rate_card: RateCard,
}

impl EchoProvider {
    pub fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            seen: Arc::new(Mutex::new(Vec::new())),
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }

    /// How many times `complete` was called.
    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// A clone of the last request the provider received, if any.
    pub fn last_request(&self) -> Option<CompletionRequest> {
        self.seen.lock().last().cloned()
    }
}

#[async_trait]
impl Provider for EchoProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().push(req.clone());
        let content = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.clone())
            .unwrap_or_default();
        Ok(CompletionResponse {
            content,
            finish_reason: FinishReason::Stop,
            usage: Usage {
                tokens_in: 1,
                tokens_out: 1,

                ..Default::default()
            },
            cost: CostTuple::default(),
            raw_provider_response: None,
        })
    }

    fn id(&self) -> ardur_runtime::ProviderId {
        ardur_runtime::ProviderId("echo".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// A provider that always fails with a fixed [`ProviderError`], and counts its
/// calls so a test can also assert it *was* reached.
pub struct ErroringProvider {
    calls: Arc<AtomicUsize>,
    rate_card: RateCard,
}

impl ErroringProvider {
    pub fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for ErroringProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(ProviderError::Upstream(
            "stub provider always fails".to_string(),
        ))
    }

    fn id(&self) -> ardur_runtime::ProviderId {
        ardur_runtime::ProviderId("erroring".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// A hook that always vetoes pre-submit with a fixed reason and priority.
pub struct VetoHook {
    id: HookId,
    priority: i32,
    reason: String,
}

impl VetoHook {
    pub fn new(id: &str, priority: i32, reason: &str) -> Self {
        Self {
            id: HookId::new(id),
            priority,
            reason: reason.to_string(),
        }
    }
}

#[async_trait]
impl LifecycleHook for VetoHook {
    async fn on_pre_submit(&self, _ctx: &PreSubmitCtx<'_>) -> HookDecision {
        HookDecision::Veto {
            reason: self.reason.clone(),
        }
    }

    fn hook_id(&self) -> HookId {
        self.id.clone()
    }

    fn priority(&self) -> i32 {
        self.priority
    }
}

/// A hook that replaces the request with a single user message of fixed
/// content.
pub struct ReplaceHook {
    id: HookId,
    priority: i32,
    replacement: String,
    model: ModelId,
}

impl ReplaceHook {
    pub fn new(id: &str, priority: i32, replacement: &str) -> Self {
        Self {
            id: HookId::new(id),
            priority,
            replacement: replacement.to_string(),
            model: test_model(),
        }
    }
}

#[async_trait]
impl LifecycleHook for ReplaceHook {
    async fn on_pre_submit(&self, ctx: &PreSubmitCtx<'_>) -> HookDecision {
        let new_request = CompletionRequest::new(
            vec![ChatMessage::user(self.replacement.clone())],
            self.model.clone(),
            ctx.request.max_tokens,
        );
        HookDecision::Replace { new_request }
    }

    fn hook_id(&self) -> HookId {
        self.id.clone()
    }

    fn priority(&self) -> i32 {
        self.priority
    }
}

/// A redaction hook: any user message containing `SECRET` is rewritten with
/// `SECRET` → `[REDACTED]` before the request reaches the provider.
pub struct RedactingHook {
    id: HookId,
    priority: i32,
}

impl RedactingHook {
    pub fn new(id: &str, priority: i32) -> Self {
        Self {
            id: HookId::new(id),
            priority,
        }
    }
}

#[async_trait]
impl LifecycleHook for RedactingHook {
    async fn on_pre_submit(&self, ctx: &PreSubmitCtx<'_>) -> HookDecision {
        if !ctx
            .request
            .messages
            .iter()
            .any(|m| m.content.contains("SECRET"))
        {
            return HookDecision::Continue;
        }
        let redacted: Vec<ChatMessage> = ctx
            .request
            .messages
            .iter()
            .map(|m| ChatMessage {
                role: m.role,
                content: m.content.replace("SECRET", "[REDACTED]"),
                tool_calls: m.tool_calls.clone(),
                tool_call_id: m.tool_call_id.clone(),
            })
            .collect();
        let mut new_request = ctx.request.clone();
        new_request.messages = redacted;
        HookDecision::Replace { new_request }
    }

    fn hook_id(&self) -> HookId {
        self.id.clone()
    }

    fn priority(&self) -> i32 {
        self.priority
    }
}

/// A post-receipt observer that captures the receipt's payload digest (hex) and
/// the response content, so a test can assert *what* got receipted.
pub struct CapturingPostReceiptHook {
    id: HookId,
    captured: Arc<Mutex<Option<Captured>>>,
}

/// What [`CapturingPostReceiptHook`] saw at post-receipt time.
#[derive(Clone)]
pub struct Captured {
    pub payload_digest_hex: String,
    pub response_content: String,
}

impl CapturingPostReceiptHook {
    pub fn new(id: &str) -> Self {
        Self {
            id: HookId::new(id),
            captured: Arc::new(Mutex::new(None)),
        }
    }

    pub fn captured(&self) -> Option<Captured> {
        self.captured.lock().clone()
    }
}

#[async_trait]
impl LifecycleHook for CapturingPostReceiptHook {
    async fn on_post_receipt(&self, ctx: &PostReceiptCtx<'_>) -> Result<(), HookError> {
        *self.captured.lock() = Some(Captured {
            payload_digest_hex: ctx.receipt.payload_digest.to_hex(),
            response_content: ctx.response.content.clone(),
        });
        Ok(())
    }

    fn hook_id(&self) -> HookId {
        self.id.clone()
    }
}
