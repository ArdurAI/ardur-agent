//! §11.0 Phase 1 — a `Composite` source merges an embedded policy and a file
//! policy into a single evaluable bundle.
use std::path::PathBuf;

use ardur_cedar_policy::{
    ActionRef, CedarPolicyBundle, Decision, EvaluationContext, PolicyBundle, PolicySource,
    PrincipalRef, ResourceRef,
};
use serde_json::json;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/composite_extra.cedar")
}

#[test]
fn composite_merges_embedded_and_file() {
    let source = PolicySource::Composite(vec![
        // Permits `tier == "free"`.
        PolicySource::Embedded(
            r#"permit (principal, action, resource) when { resource.tier == "free" };"#.to_string(),
        ),
        // Permits `tier == "gold"`.
        PolicySource::File(fixture_path()),
    ]);

    let bundle = CedarPolicyBundle::load(source).expect("composite source should load");

    // Both sources contributed exactly one policy.
    assert_eq!(bundle.policy_count(), 2);

    // A request allowed ONLY by the file-sourced policy proves the file source
    // was merged in (the embedded policy alone would deny tier=gold).
    let decision = bundle.evaluate(&EvaluationContext {
        principal: PrincipalRef("User::alice".to_string()),
        action: ActionRef("Action::Read".to_string()),
        resource: ResourceRef("Doc::readme".to_string()),
        attributes: json!({ "tier": "gold" }),
    });

    assert!(
        matches!(decision, Decision::Allow { .. }),
        "expected Allow from the file-sourced policy, got {decision:?}"
    );
}
