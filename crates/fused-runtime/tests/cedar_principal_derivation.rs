//! Phase-3 tightening: the fused runtime derives the Cedar **principal** from
//! the verified cap-token subject (stage 1) rather than accepting it as caller
//! config. These tests pin that contract:
//!
//! - the derived principal *is* the verified subject (a policy gating on
//!   `User::"<subject>"` authorizes the turn);
//! - the caller **cannot** spoof a different subject — the builder exposes no
//!   principal id, so a policy that permits only some *other* subject denies;
//! - the cap claims (audience, tools, …) ride as resource attributes, so a
//!   policy can gate on the proven facts.
//!
//! The cedar-policy crate channels evaluation attributes through the *resource*
//! entity (its Cedar `Context` is always empty), so the cap "context" surfaces
//! as `resource.<key>`, not `context.<key>` — hence the third test reads
//! `resource.audience` / `resource.tools`.

mod support;

use std::sync::Arc;

use ardur_cap_token::{
    AttenuationRule, BiscuitCapTokenAttenuator, CapScope, CapTokenAttenuator, CapTokenIssuer,
    HolderId as CapHolderId,
};
use ardur_cedar_policy::{CedarPolicyBundle, PolicyBundle, PolicySource};
use ardur_runtime::{ChatRuntime, RuntimeError};

use support::{
    AUDIENCE, EchoProvider, NOW_UNIX, TOOL, cap_issuer, gate_holder_for, generous_budget,
    mint_token_as, runtime_builder_with_policy, user_request,
};

/// Compile an embedded Cedar bundle for one test (a parse failure is a test
/// authoring bug, so it panics).
fn bundle(policy: &str) -> CedarPolicyBundle {
    CedarPolicyBundle::load(PolicySource::Embedded(policy.to_string()))
        .expect("the test policy compiles")
}

/// The runtime derives `principal = User::"alice"` from a cap-token whose
/// verified subject is `alice`. A policy that permits exactly that principal —
/// with *no* principal ever configured on the builder — authorizes the turn, so
/// the submit runs to completion.
#[tokio::test]
async fn cedar_principal_derived_from_cap_subject() {
    let provider = Arc::new(EchoProvider::new());
    let runtime = runtime_builder_with_policy(
        provider.clone(),
        bundle(r#"permit (principal == User::"alice", action, resource);"#),
    )
    // The cost gate keys the holder on the verified subject — provision `alice`.
    .provision_budget(gate_holder_for("alice"), generous_budget())
    .build()
    .expect("runtime builds");

    let token = mint_token_as("alice", AUDIENCE, &[TOOL]);
    let outcome = runtime
        .submit(user_request("authorize me", &token))
        .await
        .expect("a policy permitting the derived principal authorizes the turn");

    assert_eq!(
        outcome.response.content, "authorize me",
        "the authorized turn reached the provider and echoed the prompt"
    );
    assert_eq!(provider.call_count(), 1, "the provider was reached");
}

/// **The security-critical assertion.** The policy permits *only* `User::"alice"`,
/// but the cap-token's verified subject is `bob`. The builder exposes no way to
/// assert the principal id, so the caller cannot make the runtime evaluate as
/// `alice`: the derived principal is `User::"bob"`, no permit matches, and the
/// turn is denied before the provider is reached. A caller cannot impersonate a
/// subject the cap-token did not prove.
#[tokio::test]
async fn cedar_principal_mismatch_caller_cannot_spoof() {
    let provider = Arc::new(EchoProvider::new());
    let runtime = runtime_builder_with_policy(
        provider.clone(),
        bundle(r#"permit (principal == User::"alice", action, resource);"#),
    )
    .provision_budget(gate_holder_for("bob"), generous_budget())
    .build()
    .expect("runtime builds");

    // The cap proves `bob`; there is no builder knob to claim `alice`.
    let token = mint_token_as("bob", AUDIENCE, &[TOOL]);
    let err = runtime
        .submit(user_request("authorize me as alice", &token))
        .await
        .expect_err("a caller cannot spoof a subject the cap-token did not prove");

    assert!(
        matches!(err, RuntimeError::PolicyDenied { .. }),
        "the derived principal (bob) matches no permit (alice-only) → PolicyDenied, got {err:?}"
    );
    assert_eq!(
        provider.call_count(),
        0,
        "a denied turn never reaches the provider"
    );
}

/// The cap-token's claims surface as Cedar resource attributes: a policy gating
/// on `resource.audience` and `resource.tools` — populated from the *verified*
/// audience and tool allowlist, not from any caller-supplied attribute —
/// authorizes the turn.
#[tokio::test]
async fn cedar_context_carries_cap_claims() {
    let provider = Arc::new(EchoProvider::new());
    let runtime = runtime_builder_with_policy(
        provider.clone(),
        bundle(
            r#"permit (principal, action, resource)
               when { resource.audience == "dm:x" && resource.tools.contains("chat.submit") };"#,
        ),
    )
    // The cap is scoped to the "dm:x" audience, so the verifier must require it.
    .audience("dm:x")
    .provision_budget(gate_holder_for("carol"), generous_budget())
    .build()
    .expect("runtime builds");

    let token = mint_token_as("carol", "dm:x", &["chat.submit"]);
    let outcome = runtime
        .submit(user_request("gate on my claims", &token))
        .await
        .expect("a policy gating on the cap claims authorizes the turn");

    assert_eq!(
        outcome.response.content, "gate on my claims",
        "the claim-gated turn reached the provider"
    );
    assert_eq!(provider.call_count(), 1, "the provider was reached");
}

/// ARD-473: Cedar resource attributes must see the cap-token's *effective*
/// claims after attenuation, not the root authority claims serialized in the
/// authority context. The parent token grants `chat.submit` + `echo` and budget
/// 1000, then the child attenuates to `chat.submit` + budget 100. A Cedar policy
/// that would pass under the root claims must deny under the child claims.
#[tokio::test]
async fn cedar_resource_claims_reflect_attenuated_tools_and_budget() {
    let provider = Arc::new(EchoProvider::new());
    let runtime = runtime_builder_with_policy(
        provider.clone(),
        bundle(
            r#"permit (principal, action, resource)
               when { resource.tools.contains("echo") || resource.budget_remaining == 1000 };"#,
        ),
    )
    .provision_budget(gate_holder_for("dana"), generous_budget())
    .build()
    .expect("runtime builds");

    let parent = cap_issuer()
        .issue(
            CapHolderId("dana".to_string()),
            CapScope {
                audience: AUDIENCE.to_string(),
                expires_unix: NOW_UNIX + 3_600,
                budget_remaining: 1000,
                tool_allowlist: vec![TOOL.to_string(), "echo".to_string()],
            },
        )
        .expect("issue parent token");
    let child = BiscuitCapTokenAttenuator
        .attenuate(&parent, AttenuationRule::ReduceBudget(100).into())
        .expect("attenuate budget");
    let child = BiscuitCapTokenAttenuator
        .attenuate(
            &child,
            AttenuationRule::RestrictTools(vec![TOOL.to_string()]).into(),
        )
        .expect("attenuate tools");
    let token = child.to_base64().expect("serialize child token");

    let err = runtime
        .submit(user_request("root claims must not leak into Cedar", &token))
        .await
        .expect_err("effective claims no longer satisfy the root-claim policy");

    assert!(
        matches!(err, RuntimeError::PolicyDenied { .. }),
        "attenuated resource.tools/resource.budget_remaining should deny, got {err:?}"
    );
    assert_eq!(
        provider.call_count(),
        0,
        "a Cedar-denied turn never reaches the provider"
    );
}
