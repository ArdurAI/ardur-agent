//! Shared fixtures for the §5.0 multi-agent Phase-1 tests: a parent cap-token,
//! its issuer root key, and a runtime over the in-memory echo child runtime.
// Each test binary links this module and exercises a different subset of the
// helpers, so unused ones are expected per-binary.
#![allow(dead_code)]

use ardur_multi_agent::{
    AttenuationRule, BiscuitCapTokenIssuer, CapScope, CapToken, CapTokenIssuer,
    CapVerifyingRuntime, ChatMessage, CostEnvelope, HolderId, InMemoryMultiAgentRuntime,
    InMemoryRuntime, KeyPair, PublicKey, ReceiptId, SessionId, SubAgentRequest, SubAgentSpec,
};

/// The audience the parent token and every verification request use.
pub const AUDIENCE: &str = "agent";
/// A far-future expiry so wall-clock has no bearing on the tests.
pub const EXPIRY_UNIX: u64 = 4_000_000_000; // ~2096
/// A verification "now" comfortably before [`EXPIRY_UNIX`].
pub const NOW_UNIX: u64 = 1_700_000_000; // ~2023

/// Issue a parent cap-token granting `tools` under [`AUDIENCE`], returning the
/// token alongside the issuer root key it verifies against.
pub fn parent_token(tools: &[&str], budget: u64) -> (CapToken, PublicKey) {
    let issuer = BiscuitCapTokenIssuer::new(KeyPair::new());
    let root = issuer.public_key();
    let token = issuer
        .issue(
            HolderId("spiffe://ardur/agent/parent".to_string()),
            CapScope {
                audience: AUDIENCE.to_string(),
                expires_unix: EXPIRY_UNIX,
                budget_remaining: budget,
                tool_allowlist: tools.iter().map(|t| t.to_string()).collect(),
            },
        )
        .expect("issue parent token");
    (token, root)
}

/// A runtime over the echo child runtime, seeded with a parent token granting
/// `tools` and a fresh parent-receipt anchor (also returned so tests can assert
/// the link).
pub fn runtime_with(
    tools: &[&str],
    budget: u64,
) -> (InMemoryMultiAgentRuntime, ReceiptId, PublicKey) {
    let (token, root) = parent_token(tools, budget);
    let parent_receipt_id = ReceiptId::new();
    let runtime = InMemoryMultiAgentRuntime::in_memory(token, root, parent_receipt_id);
    (runtime, parent_receipt_id, root)
}

/// A §5.1 real-wire runtime over the [`CapVerifyingRuntime`]-wrapped echo child,
/// seeded with a parent token granting `tools` under [`AUDIENCE`]. Unlike
/// [`runtime_with`], every turn authorizes the sub-agent's attenuated token, so
/// attenuation actually gates `ask`. The parent-receipt anchor and issuer root
/// are returned alongside.
pub fn verifying_runtime_with(
    tools: &[&str],
    budget: u64,
) -> (
    InMemoryMultiAgentRuntime<CapVerifyingRuntime<InMemoryRuntime>>,
    ReceiptId,
    PublicKey,
) {
    let (token, root) = parent_token(tools, budget);
    let parent_receipt_id = ReceiptId::new();
    let runtime = InMemoryMultiAgentRuntime::verifying(AUDIENCE, token, root, parent_receipt_id);
    (runtime, parent_receipt_id, root)
}

/// A spawn spec with a fresh parent session and the given attenuation + budget.
pub fn spec(agent_id: &str, attenuation: Vec<AttenuationRule>, cents_max: u32) -> SubAgentSpec {
    SubAgentSpec {
        agent_id: agent_id.into(),
        goal: format!("delegated work for {agent_id}"),
        cap_token_attenuation: attenuation,
        cost_envelope: CostEnvelope {
            cents_max,
            ..CostEnvelope::default()
        },
        parent_session_id: SessionId::new(),
    }
}

/// A user-message ask reserving `max_cost_cents`.
pub fn ask(text: &str, max_cost_cents: u32) -> SubAgentRequest {
    SubAgentRequest {
        message: ChatMessage::user(text),
        max_cost_cents,
    }
}
