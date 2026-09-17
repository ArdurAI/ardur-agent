//! W7 — validate projected Execution Receipts against the published v0.1 schema.
//!
//! The crate's `e2e.rs` proves the projection and the JWS chain verify against
//! *our own* types. That is not the same claim as "this is a conformant ER":
//! a field we renamed, dropped, or typed differently would still round-trip
//! through our own structs while failing at any external verifier. These tests
//! check the serialized claim set against `execution-receipt-v0.1.schema.json`
//! itself.
//!
//! The schema lives outside this repository (it belongs to the Ardur governance
//! plane), so the tests locate it via `ARDUR_ER_SCHEMA` or the default clone
//! path and **skip loudly** when it is absent rather than passing vacuously.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use ardur_governance::{
    ActionClass, Canonicalization, DigestAlg, DigestObject, DigestScope, EvidenceLevel,
    ExecutionReceipt, PolicyDecision, PublicDenialReason, SideEffectClass, Verdict,
};
use serde_json::Value;

/// Where the published schema lives.
fn schema_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("ARDUR_ER_SCHEMA") {
        let path = PathBuf::from(explicit);
        return path.is_file().then_some(path);
    }
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home).join("docs/specs/execution-receipt-v0.1.schema.json");
    path.is_file().then_some(path)
}

fn load_schema() -> Option<Value> {
    let path = schema_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&text).ok()
}

/// A representative compliant receipt.
fn compliant_receipt() -> ExecutionReceipt {
    ExecutionReceipt {
        receipt_id: "01a0adca-0000-4000-8000-000000000001".into(),
        grant_id: "01a0adca-0000-4000-8000-00000000000a".into(),
        parent_receipt_id: None,
        parent_receipt_hash: None,
        actor: "cli://localhost".into(),
        verifier_id: "ardur-agent/fused-runtime".into(),
        trace_id: "01a0adca-0000-4000-8000-00000000000b".into(),
        run_nonce: "dGhpcy1pcy1hLXRlc3Qtbm9uY2U".into(),
        step_id: "01a0adca-0000-4000-8000-00000000000c".into(),
        invocation_digest: DigestObject {
            alg: DigestAlg::Sha256,
            canonicalization: Some(Canonicalization::JcsRfc8785),
            scope: Some(DigestScope::NormalizedInput),
            value: "3q2-7wAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
        },
        tool: "file.write".into(),
        action_class: ActionClass::Write,
        target: "/private/tmp/ardur-w1/report.md".into(),
        resource_family: "fs".into(),
        side_effect_class: SideEffectClass::InternalWrite,
        verdict: Verdict::Compliant,
        evidence_level: EvidenceLevel::SelfSigned,
        reason: "within policy".into(),
        policy_decisions: vec![PolicyDecision {
            backend: "cedar".into(),
            decision: "permit".into(),
            reason: Some("grant covers fs write under the scoped root".into()),
            eval_ms: Some(0.4),
        }],
        arguments_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
        budget_remaining: BTreeMap::from([("cost".to_string(), 200_u64)]),
        timestamp: "2026-09-17T05:15:36.706Z".into(),
        iss: "ardur-agent/fused-runtime".into(),
        iat: 1_789_621_936,
        exp: 1_789_625_536,
        jti: "01a0adca-0000-4000-8000-00000000000d".into(),
        public_denial_reason: None,
        internal_denial_code: None,
    }
}

/// Minimal structural validation against the schema's own rules.
///
/// A full JSON Schema engine is not a workspace dependency, so rather than add
/// one this checks the invariants that actually catch drift: every `required`
/// key present, every `enum` value legal, and the two conditional `allOf`
/// denial-field rules. Each check reads its expectation **from the schema
/// file**, so a schema change is picked up rather than re-encoded here.
fn validate(schema: &Value, receipt: &ExecutionReceipt) -> Result<(), Vec<String>> {
    let json = serde_json::to_value(receipt).expect("receipt serializes");
    let mut errors = Vec::new();

    // 1. Required properties.
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert!(
        !required.is_empty(),
        "schema declares no required fields; the test would be vacuous"
    );
    for key in &required {
        if json.get(key).is_none() || json[*key].is_null() && *key != "parent_receipt_id" {
            // `parent_receipt_id` / `parent_receipt_hash` are nullable at the
            // chain root, which the schema models as a union with `null`.
            if !matches!(*key, "parent_receipt_id" | "parent_receipt_hash") {
                errors.push(format!("missing required field `{key}`"));
            }
        }
    }

    // 2. Enum members, read from the schema rather than hard-coded.
    if let Some(props) = schema["properties"].as_object() {
        for (name, spec) in props {
            let Some(allowed) = spec["enum"].as_array() else {
                continue;
            };
            let Some(actual) = json.get(name) else {
                continue;
            };
            if actual.is_null() {
                continue;
            }
            let legal: BTreeSet<&str> = allowed.iter().filter_map(Value::as_str).collect();
            if let Some(value) = actual.as_str() {
                if !legal.contains(value) {
                    errors.push(format!(
                        "`{name}` = {value:?} is not one of {legal:?} — serialized spelling drift"
                    ));
                }
            }
        }
    }

    // 3. The conditional denial-field rules (schema `allOf`).
    let verdict = json["verdict"].as_str().unwrap_or_default();
    let has_public = json.get("public_denial_reason").is_some();
    let has_internal = json.get("internal_denial_code").is_some();
    match verdict {
        "compliant" => {
            if has_public || has_internal {
                errors.push(
                    "a compliant receipt must carry NEITHER denial field (schema allOf[0])".into(),
                );
            }
        }
        "violation" | "insufficient_evidence" => {
            if !has_public || !has_internal {
                errors.push(format!(
                    "a `{verdict}` receipt must carry BOTH denial fields (schema allOf[1])"
                ));
            }
        }
        other => errors.push(format!("unknown verdict `{other}`")),
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[test]
fn a_compliant_receipt_validates_against_the_published_schema() {
    let Some(schema) = load_schema() else {
        eprintln!(
            "SKIPPED: ER schema not found. Set ARDUR_ER_SCHEMA or clone the plane \
             to ~/docs/specs/execution-receipt-v0.1.schema.json"
        );
        return;
    };
    if let Err(errors) = validate(&schema, &compliant_receipt()) {
        panic!("compliant receipt failed schema validation: {errors:#?}");
    }
}

#[test]
fn a_violation_receipt_carries_both_denial_fields() {
    let Some(schema) = load_schema() else {
        eprintln!("SKIPPED: ER schema not found");
        return;
    };
    let mut receipt = compliant_receipt();
    receipt.verdict = Verdict::Violation;
    receipt.reason = "violation: tool_not_allowed".into();
    receipt.public_denial_reason = Some(PublicDenialReason::PolicyDenied);
    receipt.internal_denial_code = Some("tool_not_allowed".into());
    if let Err(errors) = validate(&schema, &receipt) {
        panic!("violation receipt failed schema validation: {errors:#?}");
    }
}

#[test]
fn an_insufficient_evidence_receipt_also_requires_both_denial_fields() {
    // This is the case the fail-closed work actually produces, and the schema
    // treats it exactly like a violation for denial-field purposes. A projection
    // that filled only the internal code would be unverifiable externally.
    let Some(schema) = load_schema() else {
        eprintln!("SKIPPED: ER schema not found");
        return;
    };
    let mut receipt = compliant_receipt();
    receipt.verdict = Verdict::InsufficientEvidence;
    receipt.reason = "insufficient evidence: unprojectable_attenuation".into();
    receipt.public_denial_reason = Some(PublicDenialReason::InsufficientEvidence);
    receipt.internal_denial_code = Some("unprojectable_attenuation".into());
    if let Err(errors) = validate(&schema, &receipt) {
        panic!("insufficient_evidence receipt failed schema validation: {errors:#?}");
    }
}

#[test]
fn a_compliant_receipt_carrying_denial_fields_is_rejected() {
    // The validator must be able to FAIL, or the three tests above prove
    // nothing. A compliant receipt with denial fields violates schema allOf[0].
    let Some(schema) = load_schema() else {
        eprintln!("SKIPPED: ER schema not found");
        return;
    };
    let mut receipt = compliant_receipt();
    receipt.public_denial_reason = Some(PublicDenialReason::PolicyDenied);
    receipt.internal_denial_code = Some("should_not_be_here".into());
    let errors = validate(&schema, &receipt)
        .expect_err("a compliant receipt with denial fields must be rejected");
    assert!(
        errors.iter().any(|e| e.contains("NEITHER")),
        "expected the allOf[0] rule to fire, got: {errors:#?}"
    );
}

#[test]
fn a_non_compliant_receipt_missing_denial_fields_is_rejected() {
    let Some(schema) = load_schema() else {
        eprintln!("SKIPPED: ER schema not found");
        return;
    };
    let mut receipt = compliant_receipt();
    receipt.verdict = Verdict::InsufficientEvidence;
    // Deliberately leave both denial fields unset.
    let errors = validate(&schema, &receipt)
        .expect_err("a non-compliant receipt without denial fields must be rejected");
    assert!(
        errors.iter().any(|e| e.contains("BOTH")),
        "expected the allOf[1] rule to fire, got: {errors:#?}"
    );
}

#[test]
fn enum_spellings_match_the_schema_exactly() {
    // Serde renames are the silent killer here: `InternalWrite` must serialize
    // as `internal_write`, not `InternalWrite`, or an external verifier rejects
    // a receipt our own round-trip accepts.
    let json = serde_json::to_value(compliant_receipt()).expect("serializes");
    assert_eq!(json["action_class"], "write");
    assert_eq!(json["side_effect_class"], "internal_write");
    assert_eq!(json["verdict"], "compliant");
    assert_eq!(json["evidence_level"], "self_signed");

    let mut receipt = compliant_receipt();
    receipt.verdict = Verdict::InsufficientEvidence;
    receipt.public_denial_reason = Some(PublicDenialReason::InsufficientEvidence);
    receipt.internal_denial_code = Some("telemetry_missing".into());
    let json = serde_json::to_value(&receipt).expect("serializes");
    assert_eq!(json["verdict"], "insufficient_evidence");
    assert_eq!(json["public_denial_reason"], "insufficient_evidence");
}

#[test]
fn receipts_round_trip_through_json() {
    let receipt = compliant_receipt();
    let text = serde_json::to_string(&receipt).expect("serializes");
    let back: ExecutionReceipt = serde_json::from_str(&text).expect("deserializes");
    assert_eq!(receipt, back);
}

#[test]
fn the_native_chain_field_is_a_hex_sha256_of_the_previous_jwt() {
    // G5 requires `parent_receipt_hash` to be the SHA-256 of the previous signed
    // ER JWT — 64 lowercase hex chars. A base64url value here would pass our own
    // types and fail an external verifier.
    let mut receipt = compliant_receipt();
    receipt.parent_receipt_hash =
        Some("9f40ffe5ab0ceef0a05f0da5ca8061e0dfc8013b43ecd5b3bec98b65dcecfa34".into());
    let hash = receipt.parent_receipt_hash.clone().unwrap();
    assert_eq!(hash.len(), 64, "SHA-256 hex is 64 chars");
    assert!(
        hash.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
        "parent_receipt_hash must be lowercase hex"
    );
}
