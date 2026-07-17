//! §11.0 Phase 1 — a blanket `permit` allows any request.
use ardur_cedar_policy::{
    ActionRef, CedarPolicyBundle, Decision, EvaluationContext, PolicyBundle, PolicySource,
    PrincipalRef, ResourceRef,
};
use serde_json::Value;

#[test]
fn permit_any_allows_every_request() {
    let bundle = CedarPolicyBundle::load(PolicySource::Embedded(
        "permit (principal, action, resource);".to_string(),
    ))
    .expect("embedded policy should load");

    let decision = bundle.evaluate(&EvaluationContext {
        principal: PrincipalRef("User::alice".to_string()),
        action: ActionRef("Action::Read".to_string()),
        resource: ResourceRef("Doc::readme".to_string()),
        attributes: Value::Null,
    });

    match decision {
        Decision::Allow { matched_policy_ids } => {
            assert!(
                !matched_policy_ids.is_empty(),
                "the permit policy should be reported as matched"
            );
        }
        other => panic!("expected Allow, got {other:?}"),
    }
}
