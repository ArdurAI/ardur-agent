//! §11.0 Phase 1 — a targeted `forbid` denies the matching request.
use ardur_cedar_policy::{
    ActionRef, CedarPolicyBundle, Decision, EvaluationContext, PolicyBundle, PolicySource,
    PrincipalRef, ResourceRef,
};
use serde_json::Value;

#[test]
fn forbid_alice_read_denies() {
    let bundle = CedarPolicyBundle::load(PolicySource::Embedded(
        r#"forbid (principal == User::"alice", action == Action::"Read", resource);"#.to_string(),
    ))
    .expect("embedded policy should load");

    let decision = bundle.evaluate(&EvaluationContext {
        principal: PrincipalRef("User::alice".to_string()),
        action: ActionRef("Action::Read".to_string()),
        resource: ResourceRef("Doc::readme".to_string()),
        attributes: Value::Null,
    });

    assert!(
        matches!(decision, Decision::Deny { .. }),
        "expected Deny for alice/Read, got {decision:?}"
    );
}
