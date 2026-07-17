//! §11.0 Phase 1 — `policy_count` reflects every compiled policy in a bundle.
use ardur_cedar_policy::{CedarPolicyBundle, PolicyBundle, PolicySource};

#[test]
fn counts_all_policies_in_bundle() {
    let source = r#"
        permit (principal, action, resource);
        forbid (principal, action, resource) when { resource.blocked == true };
        permit (principal, action, resource) when { resource.tier == "free" };
    "#;

    let bundle = CedarPolicyBundle::load(PolicySource::Embedded(source.to_string()))
        .expect("3-policy bundle should load");

    assert_eq!(bundle.policy_count(), 3);
}
