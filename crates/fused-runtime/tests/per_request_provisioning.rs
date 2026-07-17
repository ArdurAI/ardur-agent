//! ARD-49 — per-request provisioning ([`FusedRuntime::submit_with_provisioning`]).
//!
//! Four properties, each isolating one lever of [`PerRequestProvisioning`]:
//!
//! - a request-time budget **creates** a budget for a never-before-seen subject;
//! - a request-time budget **tops up** (additively merges onto) an existing
//!   subject's remaining balance;
//! - an **audience override** changes which audience the cap-token is verified
//!   against (so one runtime serves multiple tenant audiences);
//! - the plain `submit` path is **unchanged** — it uses the builder defaults.

mod support;

use std::sync::Arc;

use ardur_cost_gate::{CostEnvelope, CostTuple as GateCostTuple};
use ardur_fused_runtime::PerRequestProvisioning;
use ardur_runtime::{
    CapTokenRef, ChatMessage, ChatRuntime, RuntimeError, SessionId, SubmitRequest,
};

use support::{
    AUDIENCE, BillingProvider, EchoProvider, TOOL, gate_holder_for, mint_token_as, runtime_builder,
    valid_token,
};

/// A per-turn envelope that constrains only the cents axis, so a cents-only
/// budget covers it.
fn cents_envelope(cents: u32) -> CostEnvelope {
    CostEnvelope {
        cents_max: cents,
        ..Default::default()
    }
}

fn request_for_token(content: &str, token: &str) -> SubmitRequest {
    SubmitRequest {
        messages: vec![ChatMessage::user(content)],
        cap_token: CapTokenRef(token.to_string()),
        session_id: SessionId::new(),
        requested_provider: None,
    }
}

#[tokio::test]
async fn submit_with_provisioning_creates_budget_for_new_subject() {
    // A subject the runtime was never built with a budget for.
    let subject = "spiffe://ardur/user/freshly-onboarded";
    let token = mint_token_as(subject, AUDIENCE, &[TOOL]);

    let provider = Arc::new(BillingProvider::new(50));
    let runtime = runtime_builder(provider.clone())
        .projected_envelope(cents_envelope(50))
        .build()
        .expect("runtime wires");

    // Without provisioning the subject has no budget: admission is refused.
    let unfunded = runtime.submit(request_for_token("hi", &token)).await;
    assert!(
        matches!(unfunded, Err(RuntimeError::CostCeilingExceeded)),
        "an unprovisioned subject is refused, got {unfunded:?}"
    );
    assert_eq!(provider.call_count(), 0, "the refused turn never billed");

    // Provisioning 100c on the request creates the account and admits the turn.
    runtime
        .submit_with_provisioning(
            request_for_token("hi again", &token),
            PerRequestProvisioning {
                budget: Some(GateCostTuple::cents(100)),
                ..Default::default()
            },
        )
        .await
        .expect("the provisioned turn succeeds");

    // 100c provisioned − 50c spent = 50c remaining, held under the new subject.
    let remaining = runtime
        .remaining_budget(&gate_holder_for(subject))
        .await
        .expect("the subject is now provisioned");
    assert_eq!(remaining.cents, 50, "100c funded, 50c spent");
    assert_eq!(provider.call_count(), 1);
}

#[tokio::test]
async fn submit_with_provisioning_tops_up_existing_subject() {
    let subject = "spiffe://ardur/user/returning";
    let token = mint_token_as(subject, AUDIENCE, &[TOOL]);

    let provider = Arc::new(BillingProvider::new(50));
    // Build the subject with a starting balance of 50c.
    let runtime = runtime_builder(provider.clone())
        .projected_envelope(cents_envelope(50))
        .provision_budget(gate_holder_for(subject), GateCostTuple::cents(50))
        .build()
        .expect("runtime wires");

    // A plain turn spends the starting 50c down to 0.
    runtime
        .submit(request_for_token("turn one", &token))
        .await
        .expect("the first turn fits the starting 50c");
    let after_first = runtime
        .remaining_budget(&gate_holder_for(subject))
        .await
        .expect("provisioned");
    assert_eq!(after_first.cents, 0, "starting 50c fully spent");

    // A request-time top-up of 100c MERGES onto the (now 0c) balance → 100c,
    // and the turn spends 50c → 50c remaining. If the top-up had *replaced*
    // rather than merged, the result would be identical here, so the merge is
    // pinned by the next assertion: a second top-up onto a non-zero balance.
    runtime
        .submit_with_provisioning(
            request_for_token("turn two", &token),
            PerRequestProvisioning {
                budget: Some(GateCostTuple::cents(100)),
                ..Default::default()
            },
        )
        .await
        .expect("the topped-up turn succeeds");
    let after_topup = runtime
        .remaining_budget(&gate_holder_for(subject))
        .await
        .expect("provisioned");
    assert_eq!(after_topup.cents, 50, "100c top-up − 50c spend");

    // Now top up another 100c while 50c is still unspent: a merge yields 150c,
    // a replace would yield 100c. Spend 50c, then assert 100c remains — only the
    // additive merge produces this.
    runtime
        .submit_with_provisioning(
            request_for_token("turn three", &token),
            PerRequestProvisioning {
                budget: Some(GateCostTuple::cents(100)),
                ..Default::default()
            },
        )
        .await
        .expect("the second top-up turn succeeds");
    let after_second_topup = runtime
        .remaining_budget(&gate_holder_for(subject))
        .await
        .expect("provisioned");
    assert_eq!(
        after_second_topup.cents, 100,
        "merge: 50c unspent + 100c top-up − 50c spend = 100c (a replace would leave 50c)"
    );
}

#[tokio::test]
async fn submit_with_provisioning_audience_override_changes_cap_verification() {
    // The runtime's builder audience is the default AUDIENCE, but this user's
    // cap-token is scoped to a different tenant audience.
    let tenant_audience = "tenant-acme";
    let subject = "spiffe://ardur/user/acme-employee";
    let token = mint_token_as(subject, tenant_audience, &[TOOL]);

    let provider = Arc::new(EchoProvider::new());
    let runtime = runtime_builder(provider.clone())
        .projected_envelope(cents_envelope(0))
        .provision_budget(gate_holder_for(subject), GateCostTuple::cents(100))
        .build()
        .expect("runtime wires");

    // Plain submit verifies against the builder default AUDIENCE — which the
    // tenant-scoped token does not match → the cap is denied.
    let mismatched = runtime.submit(request_for_token("hello", &token)).await;
    assert!(
        matches!(mismatched, Err(RuntimeError::CapDenied { .. })),
        "the tenant token is denied under the default audience, got {mismatched:?}"
    );

    // Overriding the audience to the token's tenant audience makes the same cap
    // verify and the turn succeed.
    runtime
        .submit_with_provisioning(
            request_for_token("hello", &token),
            PerRequestProvisioning {
                audience: Some(tenant_audience.to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("the audience-override turn verifies and succeeds");
}

#[tokio::test]
async fn submit_without_provisioning_uses_builder_defaults() {
    // The unchanged happy path: the builder's HOLDER budget and AUDIENCE, a
    // valid token, and a plain `submit` — no provisioning involved.
    let provider = Arc::new(EchoProvider::new());
    let runtime = runtime_builder(provider.clone())
        .build()
        .expect("runtime wires");

    let result = runtime
        .submit(support::user_request("ping", &valid_token()))
        .await
        .expect("the default happy path still works");

    assert_eq!(result.response.content, "ping", "echo provider round-trips");
    assert_eq!(provider.call_count(), 1);
}
