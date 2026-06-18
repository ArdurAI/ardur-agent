//! Scenario §2.E (ARD-49) — `multi_tenant_budgets`.
//!
//! One boot-time [`FusedRuntime`] serves two tenants whose budgets are funded
//! **per request** ([`FusedRuntime::submit_with_provisioning`]) rather than at
//! build time. The scenario proves per-user accounting: each subject's budget is
//! tracked independently, one user exhausting their budget does not touch the
//! other's, and the cost gate refuses the over-budget turn before it bills.
//!
//! Each subject carries its own cap-token (distinct verified subjects), so the
//! budget holder is derived from the cap — no `subject` override is needed; the
//! multi-tenant separation falls out of the verified identity.
//!
//! The per-turn envelope is fixed on the runtime (admission reserves it), so
//! every turn here costs a uniform 50c. The numbers are chosen so User A's
//! budget covers exactly two turns and the third is refused, while User B —
//! funded and charged from the *same* runtime — is provably untouched.

use std::sync::Arc;

use ardur_cap_token::{CapScope, CapTokenIssuer, HolderId as CapHolderId};
use ardur_cost_gate::{CostEnvelope, CostTuple as GateCostTuple, HolderId as GateHolderId};
use ardur_e2e_tests::fixtures;
use ardur_fused_runtime::PerRequestProvisioning;
use ardur_runtime::{
    CapTokenRef, ChatMessage, ChatRuntime, RuntimeError, SessionId, SubmitRequest,
};

mod support;
use support::BillingProvider;

/// Cents each turn reserves (the runtime's fixed envelope) and bills.
const PER_TURN_CENTS: u64 = 50;

const USER_A: &str = "spiffe://ardur/tenant/acme/user-a";
const USER_B: &str = "spiffe://ardur/tenant/globex/user-b";

/// Mint a cap-token (base64) for `subject`, scoped to the fixtures' audience and
/// tool, well-funded on its own cap-token budget caveat and unexpired.
fn token_for(subject: &str) -> String {
    fixtures::dev_cap_issuer()
        .issue(
            CapHolderId(subject.to_string()),
            CapScope {
                audience: fixtures::AUDIENCE.to_string(),
                expires_unix: fixtures::NOW_UNIX + 3_600,
                budget_remaining: 1_000_000,
                tool_allowlist: vec![fixtures::TOOL.to_string()],
                capabilities: Vec::new(),
            },
        )
        .expect("the cap-token issues")
        .to_base64()
        .expect("the cap-token serializes")
}

fn request(subject_token: &str, content: &str) -> SubmitRequest {
    SubmitRequest {
        messages: vec![ChatMessage::user(content)],
        cap_token: CapTokenRef(subject_token.to_string()),
        session_id: SessionId::new(),
        requested_provider: None,
    }
}

fn fund(cents: u64) -> PerRequestProvisioning {
    PerRequestProvisioning {
        budget: Some(GateCostTuple::cents(cents)),
        ..Default::default()
    }
}

#[tokio::test]
async fn one_runtime_enforces_independent_per_tenant_budgets() {
    let provider = Arc::new(BillingProvider::new(PER_TURN_CENTS));

    // ONE runtime, built with NO initial per-tenant provisioning. The per-turn
    // envelope constrains only the cents axis so a cents-only budget covers it.
    let runtime = fixtures::fused_builder(provider.clone())
        .projected_envelope(CostEnvelope {
            cents_max: PER_TURN_CENTS as u32,
            ..Default::default()
        })
        .build()
        .expect("the fused runtime wires");

    let token_a = token_for(USER_A);
    let token_b = token_for(USER_B);
    let holder_a = GateHolderId(USER_A.to_string());
    let holder_b = GateHolderId(USER_B.to_string());

    // --- User A: first turn funds 100c on the request, then spends 50c. ---
    runtime
        .submit_with_provisioning(request(&token_a, "a-1"), fund(100))
        .await
        .expect("A's first turn provisions 100c and succeeds");
    assert_eq!(
        runtime.remaining_budget(&holder_a).await.unwrap().cents,
        50,
        "A: 100c funded − 50c spent"
    );

    // --- User B: funds 200c on the request, spends 50c — on the same runtime. ---
    runtime
        .submit_with_provisioning(request(&token_b, "b-1"), fund(200))
        .await
        .expect("B's first turn provisions 200c and succeeds");
    assert_eq!(
        runtime.remaining_budget(&holder_b).await.unwrap().cents,
        150,
        "B: 200c funded − 50c spent"
    );

    // --- User A: second turn, NO provisioning — draws on the remaining 50c. ---
    runtime
        .submit(request(&token_a, "a-2"))
        .await
        .expect("A's second turn uses the remaining 50c");
    assert_eq!(
        runtime.remaining_budget(&holder_a).await.unwrap().cents,
        0,
        "A: budget now exhausted"
    );

    let calls_before_refusal = provider.call_count();

    // --- User A: third turn — personal budget exhausted, so the gate refuses
    //     it before the provider is reached. ---
    let refused = runtime.submit(request(&token_a, "a-3")).await;
    assert!(
        matches!(refused, Err(RuntimeError::CostCeilingExceeded)),
        "A's over-budget turn is refused on cost grounds, got {refused:?}"
    );
    assert_eq!(
        provider.call_count(),
        calls_before_refusal,
        "A's refused turn never reached the provider"
    );

    // --- The crux: B's budget is completely unaffected by A's exhaustion. ---
    assert_eq!(
        runtime.remaining_budget(&holder_b).await.unwrap().cents,
        150,
        "B's 150c is untouched by A spending and over-spending"
    );

    // And B can still transact while A is locked out — independent accounting.
    runtime
        .submit(request(&token_b, "b-2"))
        .await
        .expect("B still has budget and transacts after A is exhausted");
    assert_eq!(
        runtime.remaining_budget(&holder_b).await.unwrap().cents,
        100,
        "B: 150c − 50c spent"
    );
}
