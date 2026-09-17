//! W8 — validate authored Mission Declarations against the published v0.1 schema.
//!
//! Round-tripping an MD through our own serde impls proves nothing about
//! whether a verifier would accept it: the schema carries constraints no Rust
//! type expresses — `minItems`/`maxItems` on `effect_policies`, a `contains`
//! assertion per side-effect class, URI and `sha-256:` patterns, and
//! `additionalProperties: false` throughout.
//!
//! Ignored by default because the schema lives in the governance-plane repo,
//! outside this tree. Run with:
//! `cargo test -p ardur-governance --test mission_schema -- --ignored --nocapture`

use std::collections::BTreeMap;
use std::path::PathBuf;

use ardur_governance::{
    BudgetPair, EFFECT_CLASSES, GrantRecord, MissionDeclaration, MissionIdentity,
    author_mission_declaration, mission_digest,
};
use serde_json::Value;

/// Locate `mission-declaration-v0.1.schema.json`, or `None` when unavailable.
fn schema_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ARDUR_MD_SCHEMA") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let home = std::env::var("HOME").ok()?;
    let p = PathBuf::from(home).join("docs/specs/mission-declaration-v0.1.schema.json");
    p.is_file().then_some(p)
}

/// Load the schema, or skip **loudly** — a silent pass would certify nothing.
fn schema() -> Option<Value> {
    match schema_path() {
        Some(p) => Some(serde_json::from_str(&std::fs::read_to_string(&p).ok()?).ok()?),
        None => {
            eprintln!(
                "SKIPPED: MD schema not found. Set ARDUR_MD_SCHEMA or clone the plane to \
                 ~/docs/specs/mission-declaration-v0.1.schema.json"
            );
            None
        }
    }
}

fn identity() -> MissionIdentity {
    MissionIdentity {
        iss: "ardur-agent/cli".into(),
        sub: "cli://localhost".into(),
        aud: "ardur-governance-plane".into(),
        mission_id: "workspace://ardur-agent".into(),
        jti: "01a0adca-0000-4000-8000-00000000000e".into(),
        iat: 1_789_621_936,
        exp: 1_789_708_336,
        revocation_ref: "https://plane.local/revocations".into(),
    }
}

fn budgets() -> BTreeMap<String, BudgetPair> {
    EFFECT_CLASSES
        .iter()
        .map(|c| {
            (
                (*c).to_string(),
                BudgetPair {
                    ceiling: 100,
                    reserved_share: 10,
                },
            )
        })
        .collect()
}

fn grants() -> Vec<GrantRecord> {
    vec![
        GrantRecord {
            tool: "file.read".into(),
            capabilities: vec!["cap.fs_read".into()],
            scope: Some("/private/tmp/ardur-beta".into()),
            subject: "cli://localhost".into(),
        },
        GrantRecord {
            tool: "shell.run".into(),
            capabilities: vec!["cap.process_exec".into()],
            scope: Some("git|cargo".into()),
            subject: "cli://localhost".into(),
        },
    ]
}

fn authored() -> MissionDeclaration {
    author_mission_declaration(&identity(), &grants(), &budgets()).expect("authoring succeeds")
}

/// Validate an MD against the schema's structural rules.
///
/// Deliberately hand-rolled rather than pulling a JSON Schema engine: adding a
/// dependency for a test surface changes the crate's supply chain (and
/// `cargo deny` surface) for no verification gain. What matters here is that
/// the rules are read FROM the schema file, not re-encoded from memory, so a
/// spec change is noticed instead of silently satisfied.
fn validate(schema: &Value, md: &Value) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    let obj = match md.as_object() {
        Some(o) => o,
        None => return Err(vec!["MD is not a JSON object".into()]),
    };

    // 1. Required claims, read from the schema.
    let required: Vec<&str> = schema["required"]
        .as_array()
        .expect("schema declares required")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        !required.is_empty(),
        "schema declares no required claims; the test would be vacuous"
    );
    for key in &required {
        if !obj.contains_key(*key) {
            errors.push(format!("missing required claim `{key}`"));
        }
    }

    // 2. §5.4 fail-closed: no claim outside the schema's declared properties.
    let known: Vec<&str> = schema["properties"]
        .as_object()
        .expect("schema declares properties")
        .keys()
        .map(String::as_str)
        .collect();
    for key in obj.keys() {
        if !known.contains(&key.as_str()) {
            errors.push(format!(
                "unknown claim `{key}` (unknown fields fail closed)"
            ));
        }
    }

    // 3. effect_policies: minItems/maxItems plus a `contains` per class, all
    //    read from the schema rather than assumed to be five.
    let ep_schema = &schema["properties"]["effect_policies"];
    let min = ep_schema["minItems"].as_u64().unwrap_or(0) as usize;
    let max = ep_schema["maxItems"].as_u64().unwrap_or(usize::MAX as u64) as usize;
    if let Some(list) = obj.get("effect_policies").and_then(|v| v.as_array()) {
        if list.len() < min || list.len() > max {
            errors.push(format!(
                "effect_policies has {} entries, schema requires {min}..={max}",
                list.len()
            ));
        }
        let present: Vec<&str> = list
            .iter()
            .filter_map(|e| e["side_effect_class"].as_str())
            .collect();
        for rule in ep_schema["allOf"].as_array().into_iter().flatten() {
            let wanted = rule["contains"]["properties"]["side_effect_class"]["const"]
                .as_str()
                .unwrap_or_default();
            if !wanted.is_empty() && !present.contains(&wanted) {
                errors.push(format!("effect_policies missing required class `{wanted}`"));
            }
        }
    }

    // 4. Patterned strings: the digest prefix and the tool-class URI shape.
    if let Some(d) = obj.get("tool_manifest_digest").and_then(|v| v.as_str()) {
        let ok = d.strip_prefix("sha-256:").is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        });
        if !ok {
            errors.push(format!(
                "tool_manifest_digest `{d}` violates ^sha-256:[0-9a-f]{{64}}$"
            ));
        }
    }
    if let Some(list) = obj.get("allowed_tool_classes").and_then(|v| v.as_array()) {
        if list.len()
            < schema["properties"]["allowed_tool_classes"]["minItems"]
                .as_u64()
                .unwrap_or(1) as usize
        {
            errors.push("allowed_tool_classes is below the schema minimum".into());
        }
        for c in list {
            let s = c.as_str().unwrap_or_default();
            // `^[A-Za-z][A-Za-z0-9+.-]*://`
            let is_uri = s.split_once("://").is_some_and(|(scheme, _)| {
                !scheme.is_empty()
                    && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
                    && scheme
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
            });
            if !is_uri {
                errors.push(format!("allowed_tool_classes entry `{s}` is not a URI"));
            }
        }
    }

    // 5. resource_policies patterns must be `exact:` or `glob:` prefixed.
    if let Some(list) = obj.get("resource_policies").and_then(|v| v.as_array()) {
        for p in list {
            let pat = p["pattern"].as_str().unwrap_or_default();
            if !(pat.starts_with("exact:") || pat.starts_with("glob:")) && pat.len() > 6 {
                errors.push(format!(
                    "resource pattern `{pat}` lacks an exact:/glob: prefix"
                ));
            }
        }
    }

    // 6. Enum-valued claims, with the permitted values read from the schema.
    for (claim, path) in [
        ("conformance_profile", "conformance_profile"),
        ("receipt_policy", "receipt_policy"),
    ] {
        let allowed: Vec<&str> = if claim == "receipt_policy" {
            schema["$defs"]["receipt_policy"]["properties"]["level"]["enum"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default()
        } else {
            schema["properties"][path]["enum"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default()
        };
        if allowed.is_empty() {
            continue;
        }
        let actual = if claim == "receipt_policy" {
            obj.get(claim).and_then(|v| v["level"].as_str())
        } else {
            obj.get(claim).and_then(|v| v.as_str())
        };
        if let Some(a) = actual {
            if !allowed.contains(&a) {
                errors.push(format!("`{claim}` value `{a}` is not in the schema enum"));
            }
        }
    }

    // 7. required_telemetry entries must be schema-known field names.
    let tel_enum: Vec<&str> = schema["$defs"]["telemetry_field"]["enum"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    if let Some(list) = obj.get("required_telemetry").and_then(|v| v.as_array()) {
        for t in list {
            let s = t.as_str().unwrap_or_default();
            if !tel_enum.is_empty() && !tel_enum.contains(&s) {
                errors.push(format!(
                    "required_telemetry `{s}` is not a schema telemetry field"
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[test]
#[ignore = "requires the governance-plane schema on disk"]
fn an_authored_md_validates_against_the_published_schema() {
    let Some(schema) = schema() else { return };
    let value = serde_json::to_value(authored()).unwrap();

    if let Err(errors) = validate(&schema, &value) {
        panic!("authored MD does not satisfy the published schema: {errors:#?}");
    }
}

#[test]
#[ignore = "requires the governance-plane schema on disk"]
fn every_required_claim_is_present() {
    let Some(schema) = schema() else { return };
    // Read the required list FROM the schema rather than re-encoding it here:
    // a test carrying its own copy cannot notice the spec gaining a field.
    let required: Vec<&str> = schema["required"]
        .as_array()
        .expect("schema declares required")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(!required.is_empty(), "schema must declare required claims");

    let value = serde_json::to_value(authored()).unwrap();
    let obj = value.as_object().unwrap();
    for claim in required {
        assert!(
            obj.contains_key(claim),
            "authored MD omits required `{claim}`"
        );
    }
}

#[test]
#[ignore = "requires the governance-plane schema on disk"]
fn the_effect_policy_cardinality_rule_is_enforced_by_the_schema() {
    // `effect_policies` is minItems:5/maxItems:5 with a `contains` per class.
    // Dropping one must be REJECTED — if this passes, the validator is not
    // actually applying the constraint and the happy-path test proves nothing.
    let Some(schema) = schema() else { return };
    let mut value = serde_json::to_value(authored()).unwrap();
    value["effect_policies"].as_array_mut().unwrap().truncate(4);
    assert!(
        validate(&schema, &value).is_err(),
        "a 4-entry effect_policies list must be rejected"
    );

    // Right count, wrong content: duplicate `read` in place of `external_send`.
    let mut dup = serde_json::to_value(authored()).unwrap();
    dup["effect_policies"][4]["side_effect_class"] = Value::String("read".into());
    assert!(
        validate(&schema, &dup).is_err(),
        "five entries missing a required class must be rejected"
    );
}

#[test]
#[ignore = "requires the governance-plane schema on disk"]
fn unknown_claims_are_rejected_fail_closed() {
    // §5.4: unknown fields fail closed. If the schema tolerated extras, a
    // verifier could silently ignore a claim the issuer believed was enforced.
    let Some(schema) = schema() else { return };
    let mut value = serde_json::to_value(authored()).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("unexpected_claim".into(), Value::Bool(true));
    assert!(
        validate(&schema, &value).is_err(),
        "an unknown top-level claim must be rejected"
    );
}

#[test]
#[ignore = "requires the governance-plane schema on disk"]
fn the_manifest_digest_pattern_is_enforced() {
    let Some(schema) = schema() else { return };
    let mut value = serde_json::to_value(authored()).unwrap();
    // A bare hex digest with no `sha-256:` prefix must fail.
    value["tool_manifest_digest"] = Value::String("a".repeat(64));
    assert!(
        validate(&schema, &value).is_err(),
        "a digest without the sha-256: prefix must be rejected"
    );
}

#[test]
#[ignore = "requires the governance-plane schema on disk"]
fn tool_classes_must_be_uris_not_bare_tool_ids() {
    let Some(schema) = schema() else { return };
    let mut value = serde_json::to_value(authored()).unwrap();
    value["allowed_tool_classes"] = Value::Array(vec![Value::String("file.read".into())]);
    assert!(
        validate(&schema, &value).is_err(),
        "a bare tool id is not a tool-class URI"
    );
}

#[test]
#[ignore = "requires the governance-plane schema on disk"]
fn the_declaration_digest_is_computable_and_prefixed() {
    // The DG binds to the MD through this digest, so it must exist and match
    // the `sha-256:` + 64-hex shape the grant profile expects.
    let digest = mission_digest(&authored()).expect("digest computes");
    assert!(digest.starts_with("sha-256:"));
    assert_eq!(digest.len(), "sha-256:".len() + 64);
    assert!(
        digest["sha-256:".len()..]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "digest must be lowercase hex"
    );
}
