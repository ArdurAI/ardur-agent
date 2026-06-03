//! Shared scaffolding for the fused-runtime integration tests: a deterministic
//! cap-token issuer/root, a receipt key, permissive + deny-all policy bundles,
//! stub providers (echoing, billing a fixed cost, always-erroring), and a
//! one-line builder helper that wires a `FusedRuntime` with sensible test
//! defaults.

#![allow(dead_code)] // each test file uses a different subset.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ardur_cap_token::{
    BiscuitCapTokenIssuer, CapScope, CapTokenIssuer, HolderId as CapHolderId, KeyPair, PublicKey,
};
use ardur_cedar_policy::{CedarPolicyBundle, PolicyBundle, PolicySource};
use ardur_cost_gate::{Clock, CostTuple as GateCostTuple, HolderId as GateHolderId, ManualClock};
use ardur_fused_runtime::FusedRuntimeBuilder;
use ardur_lifecycle_hooks::{HookDecision, HookId, LifecycleHook, PreSubmitCtx};
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, ModelId, Provider, ProviderError,
    RateCard, Usage,
};
use ardur_receipt::Es256SigningKey;
use ardur_runtime::{CapTokenRef, ChatMessage, CostTuple, Role, SessionId, SubmitRequest};
use async_trait::async_trait;
use biscuit_auth::PrivateKey;
use parking_lot::Mutex;

/// The audience every test cap-token is scoped to (and the runtime requires).
pub const AUDIENCE: &str = "ardur-cli";
/// The tool/capability name the turn exercises.
pub const TOOL: &str = "chat.submit";
/// The SPIFFE-style holder the cap-token is issued to and the budget held under.
pub const HOLDER: &str = "spiffe://ardur/user/test";
/// A fixed "now" (seconds) — well before the default token expiry.
pub const NOW_UNIX: u64 = 1_750_000_000;
/// The same instant in milliseconds, for the cost-gate clock and timestamps.
pub const NOW_MS: u64 = 1_750_000_000_000;
/// The model the runtime dispatches under.
pub const TEST_MODEL: &str = "claude-opus-4-8";

/// A fixed 32-byte Ed25519 seed for the cap-token root key, so `cap_issuer` and
/// `cap_root` agree on every call.
const CAP_ROOT_SEED: [u8; 32] = [
    0x2e, 0x45, 0x31, 0xa7, 0x0c, 0x9b, 0x6d, 0x14, 0xf3, 0x88, 0x21, 0x5c, 0xbe, 0x47, 0x90, 0xd2,
    0x6a, 0x1f, 0x33, 0x70, 0xc8, 0x05, 0x9e, 0x42, 0xab, 0x77, 0x18, 0xe9, 0x54, 0x3c, 0x6b, 0x0d,
];

/// The deterministic cap-token issuer (root key from [`CAP_ROOT_SEED`]).
pub fn cap_issuer() -> BiscuitCapTokenIssuer {
    let private =
        PrivateKey::from_bytes(&CAP_ROOT_SEED).expect("CAP_ROOT_SEED is a valid Ed25519 key");
    BiscuitCapTokenIssuer::new(KeyPair::from(&private))
}

/// The cap-token root public key the verifier checks against.
pub fn cap_root() -> PublicKey {
    cap_issuer().public_key()
}

/// A fresh receipt signing key (determinism across calls is unnecessary for the
/// in-process tests — the chain links are re-derived from the JWS bytes).
pub fn receipt_key() -> Es256SigningKey {
    Es256SigningKey::generate()
}

/// A permissive Cedar bundle — one unconditional `permit` — so the
/// authorization seam says `Allow`.
pub fn permissive_policy() -> CedarPolicyBundle {
    CedarPolicyBundle::load(PolicySource::Embedded(
        "permit(principal, action, resource);".to_string(),
    ))
    .expect("the permissive policy compiles")
}

/// A deny-all Cedar bundle — one unconditional `forbid` — so the authorization
/// seam says `Deny`.
pub fn deny_all_policy() -> CedarPolicyBundle {
    CedarPolicyBundle::load(PolicySource::Embedded(
        "forbid(principal, action, resource);".to_string(),
    ))
    .expect("the deny-all policy compiles")
}

/// Mint a cap-token (base64) for [`HOLDER`], scoped to [`AUDIENCE`] / [`TOOL`].
pub fn mint_token(expires_unix: u64, budget_remaining: u64) -> String {
    cap_issuer()
        .issue(
            CapHolderId(HOLDER.to_string()),
            CapScope {
                audience: AUDIENCE.to_string(),
                expires_unix,
                budget_remaining,
                tool_allowlist: vec![TOOL.to_string()],
            },
        )
        .expect("the cap-token issues")
        .to_base64()
        .expect("the cap-token serializes")
}

/// A valid (non-expired, well-funded) cap-token for the happy path.
pub fn valid_token() -> String {
    mint_token(NOW_UNIX + 3_600, 1_000_000)
}

/// Mint a cap-token (base64) for an arbitrary subject / audience / tool
/// allowlist — the lever the Cedar-derivation tests pull to prove the runtime
/// authorizes as the *verified* subject (not a caller-asserted principal). The
/// expiry is well ahead of [`NOW_UNIX`] and the budget is generous, so the token
/// clears stage-1 verification and the only variable is the claim under test.
pub fn mint_token_as(subject: &str, audience: &str, tools: &[&str]) -> String {
    cap_issuer()
        .issue(
            CapHolderId(subject.to_string()),
            CapScope {
                audience: audience.to_string(),
                expires_unix: NOW_UNIX + 3_600,
                budget_remaining: 1_000_000,
                tool_allowlist: tools.iter().map(|t| (*t).to_string()).collect(),
            },
        )
        .expect("the cap-token issues")
        .to_base64()
        .expect("the cap-token serializes")
}

/// The gate holder id for an arbitrary subject (so a test can provision budget
/// for the subject its cap-token is minted under — the cost gate keys the
/// holder on the verified subject).
pub fn gate_holder_for(subject: &str) -> GateHolderId {
    GateHolderId(subject.to_string())
}

/// A deterministic manual clock pinned at [`NOW_MS`].
pub fn manual_clock() -> Arc<dyn Clock> {
    Arc::new(ManualClock::new(NOW_MS))
}

/// A budget that comfortably covers the default envelope.
pub fn generous_budget() -> GateCostTuple {
    GateCostTuple {
        tokens_in: 1_000_000_000,
        tokens_out: 1_000_000_000,
        cents: 1_000_000,
        wall_ms: 1_000_000_000,
        attention_score: 1_000_000_000,
    }
}

/// The gate holder id for [`HOLDER`].
pub fn gate_holder() -> GateHolderId {
    GateHolderId(HOLDER.to_string())
}

/// A builder pre-wired with the deterministic root, the given policy bundle, a
/// manual clock, audience/tool, and a generous budget.
pub fn runtime_builder_with_policy(
    provider: Arc<dyn Provider>,
    policy: CedarPolicyBundle,
) -> FusedRuntimeBuilder {
    FusedRuntimeBuilder::new(
        cap_root(),
        policy,
        provider,
        receipt_key(),
        ModelId::new(TEST_MODEL),
    )
    .audience(AUDIENCE)
    .tool(TOOL)
    .clock(manual_clock())
    .provision_budget(gate_holder(), generous_budget())
}

/// The happy-path baseline: [`runtime_builder_with_policy`] over the permissive
/// bundle. Tests then tweak it.
pub fn runtime_builder(provider: Arc<dyn Provider>) -> FusedRuntimeBuilder {
    runtime_builder_with_policy(provider, permissive_policy())
}

/// A `SubmitRequest` for a single user message + cap-token, with a fresh session.
pub fn user_request(content: &str, cap_token: &str) -> SubmitRequest {
    request_for(content, cap_token, SessionId::new())
}

/// A `SubmitRequest` bound to a specific session (so a file journal keyed on it
/// can be replayed).
pub fn request_for(content: &str, cap_token: &str, session_id: SessionId) -> SubmitRequest {
    SubmitRequest {
        messages: vec![ChatMessage::user(content)],
        cap_token: CapTokenRef(cap_token.to_string()),
        session_id,
        requested_provider: None,
    }
}

/// A provider that echoes the last user message, recording every request and
/// counting calls. Bills nothing (zero-cost stub).
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

    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

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

/// A provider that bills a fixed `cents` per call (other dimensions zero), so a
/// finite budget actually depletes across turns. Echoes the prompt.
pub struct BillingProvider {
    cents: u64,
    calls: Arc<AtomicUsize>,
    rate_card: RateCard,
}

impl BillingProvider {
    pub fn new(cents: u64) -> Self {
        Self {
            cents,
            calls: Arc::new(AtomicUsize::new(0)),
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for BillingProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
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
                tokens_in: 0,
                tokens_out: 0,
            },
            cost: CostTuple {
                tokens_in: 0,
                tokens_out: 0,
                cents: self.cents,
                wall_ms: 0,
                attention_score: 0.0,
            },
            raw_provider_response: None,
        })
    }

    fn id(&self) -> ardur_runtime::ProviderId {
        ardur_runtime::ProviderId("billing".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// A provider that always fails, counting its calls.
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

/// A pre-submit hook that always vetoes with a fixed reason.
pub struct VetoHook {
    id: HookId,
    reason: String,
}

impl VetoHook {
    pub fn new(id: &str, reason: &str) -> Self {
        Self {
            id: HookId::new(id),
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
}

/// A pre-submit hook that rewrites `SECRET` → `[REDACTED]` in every message
/// before the request reaches the provider.
pub struct RedactingHook {
    id: HookId,
}

impl RedactingHook {
    pub fn new(id: &str) -> Self {
        Self {
            id: HookId::new(id),
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
        let mut new_request = ctx.request.clone();
        new_request.messages = ctx
            .request
            .messages
            .iter()
            .map(|m| ChatMessage {
                role: m.role,
                content: m.content.replace("SECRET", "[REDACTED]"),
            })
            .collect();
        HookDecision::Replace { new_request }
    }

    fn hook_id(&self) -> HookId {
        self.id.clone()
    }
}
