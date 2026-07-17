//! §11.0 Phase 1 — a `when { resource.tier == "free" }` clause matches only
//! when the resource attribute is present and equal; otherwise the permit
//! falls through to an implicit deny.
use ardur_cedar_policy::{
    ActionRef, CedarPolicyBundle, Decision, EvaluationContext, PolicyBundle, PolicySource,
    PrincipalRef, ResourceRef,
};
use serde_json::json;

fn free_tier_bundle() -> CedarPolicyBundle {
    CedarPolicyBundle::load(PolicySource::Embedded(
        r#"permit (principal, action, resource) when { resource.tier == "free" };"#.to_string(),
    ))
    .expect("embedded policy should load")
}

fn ctx_with_tier(tier: &str) -> EvaluationContext {
    EvaluationContext {
        principal: PrincipalRef("User::alice".to_string()),
        action: ActionRef("Action::Read".to_string()),
        resource: ResourceRef("Doc::readme".to_string()),
        attributes: json!({ "tier": tier }),
    }
}

#[test]
fn free_tier_satisfies_the_when_clause() {
    let decision = free_tier_bundle().evaluate(&ctx_with_tier("free"));
    assert!(
        matches!(decision, Decision::Allow { .. }),
        "expected Allow for tier=free, got {decision:?}"
    );
}

#[test]
fn paid_tier_falls_through_to_implicit_deny() {
    let decision = free_tier_bundle().evaluate(&ctx_with_tier("paid"));
    assert!(
        matches!(decision, Decision::Deny { .. }),
        "expected implicit Deny for tier=paid, got {decision:?}"
    );
}
