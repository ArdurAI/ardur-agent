//! #545 — validate the shared effect-bucket registry against the SELECTED
//! public plane schemas, reading the normative vocabulary FROM the schema
//! files rather than re-encoding it here.
//!
//! The registry is only "the normative five classes" by assertion; this test
//! makes that claim checkable against the plane's published artifacts:
//!
//! - `mission-declaration-v0.1.schema.json` pins the class set twice — in
//!   `effect_policies` (a `contains` per class) and in
//!   `lineage_budgets.per_effect_class` (a required key per class). The
//!   registry must equal BOTH.
//! - `execution-receipt-v0.1.schema.json` constrains every
//!   `budget_remaining` key with `propertyNames.pattern`; every registry
//!   class name must satisfy it, or an adapter-projected receipt fails at
//!   any external verifier.
//!
//! Ignored by default (the schemas live in the governance-plane repo, outside
//! this tree); the CI `schema conformance` job runs it for real and FAILS if
//! it skips. Run locally:
//! `cargo test -p ardur-governance --test effect_schema_conformance -- --ignored`

use std::path::PathBuf;

use ardur_governance::{
    EffectClass, REGISTRY_EFFECT_CLASSES, effect_bucket_registry, normalize_effect_class,
};

fn schema_path(env: &str, file: &str) -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var(env) {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home).join("docs/specs").join(file);
    path.is_file().then_some(path)
}

fn load(env: &str, file: &str) -> Option<serde_json::Value> {
    let path = schema_path(env, file)?;
    // Exists-but-broken is a hard failure, not an absent schema.
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("schema at {} could not be read: {e}", path.display()));
    Some(
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("schema at {} is not valid JSON: {e}", path.display())),
    )
}

/// The classes the MD schema's `effect_policies.allOf[].contains` rules
/// demand — parsed out of the schema, never re-encoded.
fn classes_from_effect_policies(md: &serde_json::Value) -> Vec<String> {
    let rules = md["properties"]["effect_policies"]["allOf"]
        .as_array()
        .expect("effect_policies declares allOf contains rules");
    let mut classes: Vec<String> = rules
        .iter()
        .map(|rule| {
            rule["contains"]["properties"]["side_effect_class"]["const"]
                .as_str()
                .expect("a contains rule pins a const class")
                .to_string()
        })
        .collect();
    classes.sort();
    classes
}

/// The classes `lineage_budgets.per_effect_class` requires as keys.
fn classes_from_lineage_budgets(md: &serde_json::Value) -> Vec<String> {
    let mut classes: Vec<String> = md["$defs"]["lineage_budgets"]["properties"]["per_effect_class"]
        ["required"]
        .as_array()
        .expect("per_effect_class declares required keys")
        .iter()
        .map(|v| v.as_str().expect("a class name").to_string())
        .collect();
    classes.sort();
    classes
}

#[test]
#[ignore = "requires the governance-plane schema on disk"]
fn the_registry_class_set_equals_the_md_schema_namespace() {
    let Some(md) = load("ARDUR_MD_SCHEMA", "mission-declaration-v0.1.schema.json") else {
        eprintln!("SKIPPED: MD schema not found. Set ARDUR_MD_SCHEMA");
        return;
    };
    let registry = effect_bucket_registry();

    // Both schema surfaces must agree with each other first — if they ever
    // diverge, the plane's own spec is inconsistent and this test must not
    // paper over it by matching only one.
    let from_policies = classes_from_effect_policies(&md);
    let from_budgets = classes_from_lineage_budgets(&md);
    assert_eq!(
        from_policies, from_budgets,
        "the MD schema's two effect-class surfaces disagree with each other"
    );

    let mut registry_names: Vec<String> =
        registry.classes().iter().map(|c| c.to_string()).collect();
    registry_names.sort();
    assert_eq!(
        registry_names, from_policies,
        "the shared registry must carry exactly the schema's effect-class namespace"
    );

    // And the typed constant equals the string constant the MD author uses.
    let mut typed: Vec<String> = REGISTRY_EFFECT_CLASSES
        .iter()
        .map(|c| c.to_string())
        .collect();
    typed.sort();
    assert_eq!(typed, from_policies);
}

#[test]
#[ignore = "requires the governance-plane schema on disk"]
fn every_registry_class_is_a_legal_budget_remaining_key() {
    let Some(er) = load("ARDUR_ER_SCHEMA", "execution-receipt-v0.1.schema.json") else {
        eprintln!("SKIPPED: ER schema not found. Set ARDUR_ER_SCHEMA");
        return;
    };
    let pattern = er["properties"]["budget_remaining"]["propertyNames"]["pattern"]
        .as_str()
        .expect("budget_remaining declares a propertyNames pattern");
    // The schema pattern is an anchored char-class + length bound
    // (`^[A-Za-z0-9._:-]{1,64}$`). Implement exactly that grammar — ranges
    // `X-Y` and literals inside `[...]`, then `{m,n}` — rather than pulling
    // a regex dependency for one test.
    let anchored = pattern
        .strip_prefix('^')
        .and_then(|p| p.strip_suffix('$'))
        .unwrap_or(pattern);
    let (class_spec, len_spec) = anchored
        .strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
        .expect("a char class");
    let (min_len, max_len) = len_spec
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .and_then(|s| s.split_once(','))
        .map_or((1usize, 64usize), |(m, n)| {
            (
                m.parse().expect("min length"),
                n.parse().expect("max length"),
            )
        });
    let allowed = |ch: char| -> bool {
        let bytes = class_spec.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            // A range `X-Y` (three bytes, literal dash in the middle).
            if i + 2 < bytes.len() && bytes[i + 1] == b'-' {
                let lo = bytes[i];
                let hi = bytes[i + 2];
                let b = u8::try_from(ch).unwrap_or(0);
                if lo <= b && b <= hi {
                    return true;
                }
                i += 3;
            } else {
                if char::from(bytes[i]) == ch {
                    return true;
                }
                i += 1;
            }
        }
        false
    };
    for class in effect_bucket_registry().classes() {
        let name = class.to_string();
        assert!(
            name.chars().count() >= min_len && name.len() <= max_len,
            "`{name}` violates the budget_remaining key length bound {pattern}"
        );
        for ch in name.chars() {
            assert!(
                allowed(ch),
                "`{name}` contains `{ch}`, outside the budget_remaining key charset {pattern}"
            );
        }
    }
}

#[test]
#[ignore = "requires the governance-plane schema on disk"]
fn the_md_authored_under_the_registry_validates_against_the_schema_budget_rules() {
    // End-to-end through the MD author: budgets keyed by the registry classes
    // must produce a lineage_budgets object the schema accepts (every
    // required key present, none extra — additionalProperties:false).
    let Some(md) = load("ARDUR_MD_SCHEMA", "mission-declaration-v0.1.schema.json") else {
        eprintln!("SKIPPED: MD schema not found. Set ARDUR_MD_SCHEMA");
        return;
    };
    use std::collections::BTreeMap;
    let identity = ardur_governance::MissionIdentity {
        iss: "ardur-agent/cli".into(),
        sub: "cli://localhost".into(),
        aud: "ardur-governance-plane".into(),
        mission_id: "workspace://ardur-agent".into(),
        jti: "01a0adca-0000-4000-8000-00000000000e".into(),
        iat: 1_789_621_936,
        exp: 1_789_708_336,
        revocation_ref: "https://plane.local/revocations".into(),
    };
    let grants = vec![ardur_governance::GrantRecord {
        tool: "file.read".into(),
        capabilities: vec!["cap.fs_read".into()],
        scope: Some("/private/tmp/ardur-beta".into()),
        subject: "cli://localhost".into(),
        receipt_id: Some("receipt-a".into()),
    }];
    let budgets: BTreeMap<String, ardur_governance::BudgetPair> = REGISTRY_EFFECT_CLASSES
        .iter()
        .map(|c| {
            (
                c.to_string(),
                ardur_governance::BudgetPair {
                    ceiling: 100,
                    reserved_share: 10,
                },
            )
        })
        .collect();
    let authored = ardur_governance::author_mission_declaration(&identity, &grants, &budgets)
        .expect("authors under registry keys");

    let value = serde_json::to_value(&authored).expect("serializes");
    let per_class = &value["lineage_budgets"]["per_effect_class"];
    for class in classes_from_lineage_budgets(&md) {
        assert!(
            per_class.get(&class).is_some(),
            "authored MD omits required budget key `{class}`"
        );
    }
    let allowed_keys = classes_from_lineage_budgets(&md);
    for key in per_class.as_object().expect("an object").keys() {
        assert!(
            allowed_keys.contains(key),
            "authored MD carries non-schema budget key `{key}` (additionalProperties: false)"
        );
    }
}

#[test]
#[ignore = "requires the governance-plane schema on disk"]
fn normalization_covers_the_observed_event_wire_vocabulary() {
    // The §6.2 side_effect_class enum the ObservedEvent schema-adjacent
    // crate serializes must ALL normalize: an emitter spelling with no
    // bucket would be an unbudgeted effect at the plane (§6.3 rule 4 →
    // insufficient_evidence), and the shared registry exists precisely to
    // prevent that class of gap.
    let Some(_md) = load("ARDUR_MD_SCHEMA", "mission-declaration-v0.1.schema.json") else {
        eprintln!("SKIPPED: MD schema not found. Set ARDUR_MD_SCHEMA");
        return;
    };
    use ardur_observed_events::SideEffectClass as Observed;
    let wire_spellings = [
        Observed::None,
        Observed::InternalWrite,
        Observed::ExternalSend,
        Observed::StateChange,
    ];
    // Exhaustive over the emitter enum: a new variant must be added here,
    // which is exactly the forced explicit decision the registry wants.
    #[deny(unreachable_patterns)]
    match Observed::None {
        Observed::None
        | Observed::InternalWrite
        | Observed::ExternalSend
        | Observed::StateChange => {}
    }
    for variant in wire_spellings {
        let wire = serde_json::to_value(variant)
            .expect("serializes")
            .as_str()
            .expect("a string enum")
            .to_string();
        assert!(
            normalize_effect_class(&wire).is_some(),
            "emitter spelling `{wire}` has no registry bucket"
        );
    }
    // Normative classes normalize too (fixed points).
    for class in REGISTRY_EFFECT_CLASSES {
        assert_eq!(normalize_effect_class(&class.to_string()), Some(class));
    }
    let _ = EffectClass::Read;
}
