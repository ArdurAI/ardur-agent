//! #535 red-first guard tests — required telemetry must be evidenced by a
//! carried, usable value, never by a recognized name.
//!
//! These contrast tables pin the checker's behaviour for every telemetry field
//! the §6.2 event actually carries, plus the two spec fields the current
//! serialized type does NOT carry (`content_provenance`, `confidence_hint`)
//! and the delegation fields whose presence was being INFERRED from
//! unrelated fields (`delegation_to` from `downstream_receipt_ids`).
//!
//! Proven RED against the pre-fix `has_usable_telemetry` (the unconditional
//! `true` arm and the `downstream_receipt_ids` inference), then green with the
//! narrow fix. A deny-all rewrite cannot pass: `compliant` controls
//! (`tool_name`, `delegation_to` present, …) require `Ok(Compliant)`.
//!
//! The serialized form is inspected directly (serde_json::to_value) so the
//! guard judges the event a real consumer would see, not the in-memory type.

use ardur_observed_events::{
    AuditCode, DelegationEdge, LineageContext, ObservedEventBuilder, SideEffectClass, Verdict,
    Visibility,
};
use std::collections::BTreeMap;

/// Baseline: a step that IS fully evidenced, so required-field contrasts are
/// isolated from every other §9 rule (visibility, envelope, manifest, budget).
fn fully_evidenced_event() -> ObservedEventBuilder {
    ObservedEventBuilder::new(
        "sess-1",
        "cli://localhost",
        "grant-1",
        "file.write",
        "write",
        "/tmp/scratch/out.txt",
        "fs",
        SideEffectClass::InternalWrite,
    )
    .budget("tool_exec", 1)
    .visibility(Visibility::Full)
    .content("text", "low", false)
    .summary("wrote 12 bytes")
    .envelope(true, "digest-abc")
}

/// Baseline lineage context: digest matches, nothing revoked, budget tracked.
fn base_ctx() -> LineageContext {
    LineageContext {
        declared_manifest_digest: "digest-abc".into(),
        required_telemetry: vec![],
        revoked: false,
        remaining_budget: BTreeMap::from([("tool_exec".to_string(), 10)]),
    }
}

/// Require exactly one telemetry field on an otherwise fully evidenced event.
fn require(field: &str) -> LineageContext {
    let mut ctx = base_ctx();
    ctx.required_telemetry = vec![field.to_string()];
    ctx
}

/// The #535 failure signature: TelemetryMissing -> insufficient_evidence.
fn insufficient(v: &Result<Verdict, (Verdict, AuditCode)>) -> bool {
    matches!(
        v,
        Err((Verdict::InsufficientEvidence, AuditCode::TelemetryMissing))
    )
}

/// #535's core defect: the serialized event carries neither
/// `content_provenance` nor `confidence_hint`, so requiring either must be
/// `insufficient_evidence` (TelemetryMissing), never `Ok(Compliant)`.
#[test]
fn required_telemetry_absent_from_the_serialized_event_yields_insufficient_evidence() {
    for field in ["content_provenance", "confidence_hint"] {
        let event = fully_evidenced_event().build();
        let json = serde_json::to_value(&event).expect("serializes");
        assert!(
            json.get(field).is_none(),
            "control failed: `{field}` unexpectedly carried by the event; \
             pick a genuinely absent field for this contrast"
        );
        let verdict = event.local_verdict(&require(field));
        assert!(
            insufficient(&verdict),
            "`{field}`: required telemetry absent from the observed event must be \
             insufficient_evidence/TelemetryMissing, got {verdict:?} — fabricating \
             compliant from a recognized name is #535's defect"
        );
    }
}

/// The name `delegation_to` must be evidenced by the actual `delegation_to`
/// value, not inferred from `downstream_receipt_ids` (a DIFFERENT §6.2 field).
#[test]
fn delegation_to_is_evidenced_by_its_own_field_not_by_downstream_receipts() {
    // (a) delegation_to ABSENT, downstream receipts PRESENT — the false
    //     compliance row from the issue: pre-fix this read Ok(Compliant).
    let event = fully_evidenced_event()
        .delegation(DelegationEdge {
            delegation_from: None,
            delegation_to: None,
            parent_event_id: None,
            parent_receipt_id: None,
            downstream_receipt_ids: vec!["child-receipt-1".into()],
        })
        .build();
    let verdict = event.local_verdict(&require("delegation_to"));
    assert!(
        insufficient(&verdict),
        "delegation_to=null with downstream receipts: must be insufficient_evidence \
         (the receipts are a different field), got {verdict:?}"
    );

    // (b) delegation_to PRESENT, downstream receipts ABSENT — the false
    //     insufficiency row from the issue: pre-fix this wrongly failed.
    let event = fully_evidenced_event()
        .delegation(DelegationEdge {
            delegation_from: None,
            delegation_to: Some("child-grant".into()),
            parent_event_id: None,
            parent_receipt_id: None,
            downstream_receipt_ids: vec![],
        })
        .build();
    assert_eq!(
        event.local_verdict(&require("delegation_to")),
        Ok(Verdict::Compliant),
        "a carried nonblank delegation_to must satisfy its own requirement"
    );

    // (c) blank string is not a usable value either (§9.2).
    let event = fully_evidenced_event()
        .delegation(DelegationEdge {
            delegation_from: None,
            delegation_to: Some("   ".into()),
            parent_event_id: None,
            parent_receipt_id: None,
            downstream_receipt_ids: vec![],
        })
        .build();
    let verdict = event.local_verdict(&require("delegation_to"));
    assert!(
        insufficient(&verdict),
        "a blank delegation_to is structurally unusable, got {verdict:?}"
    );
}

/// Independence controls for the other Option<String> delegation fields:
/// requiring them when they are None must fail closed; present values satisfy.
#[test]
fn parent_and_from_fields_are_evidenced_by_their_own_values() {
    // absent → insufficient
    let plain = fully_evidenced_event().build();
    for field in ["parent_event_id", "delegation_from"] {
        let json = serde_json::to_value(&plain).expect("serializes");
        assert_eq!(
            json.get(field),
            Some(&serde_json::Value::Null),
            "control: `{field}` should serialize as null on the baseline event"
        );
        let verdict = plain.local_verdict(&require(field));
        assert!(
            insufficient(&verdict),
            "`{field}`=null must not satisfy a requirement naming it, got {verdict:?}"
        );
    }
    // present → compliant
    let linked = fully_evidenced_event()
        .delegation(DelegationEdge {
            delegation_from: Some("parent-grant".into()),
            delegation_to: None,
            parent_event_id: Some("evt-parent".into()),
            parent_receipt_id: None,
            downstream_receipt_ids: vec![],
        })
        .build();
    for field in ["parent_event_id", "delegation_from"] {
        assert_eq!(
            linked.local_verdict(&require(field)),
            Ok(Verdict::Compliant),
            "a carried `{field}` must satisfy its own requirement"
        );
    }
}

/// Positive control so a deny-all rewrite cannot pass this suite: a supported
/// field with a real value still yields `Ok(Compliant)`.
#[test]
fn supported_telemetry_with_a_real_value_still_yields_compliant() {
    let event = fully_evidenced_event().build();
    for field in [
        "event_id",
        "session_id",
        "timestamp",
        "actor",
        "grant_id",
        "tool_name",
        "action_class",
        "target",
        "resource_family",
        "summary",
        "content_class",
        "sensitivity",
        "observed_manifest_digest",
        "budget_delta",
        "side_effect_class",
        "visibility",
        "instruction_bearing",
        "envelope_signature_valid",
    ] {
        assert_eq!(
            event.local_verdict(&require(field)),
            Ok(Verdict::Compliant),
            "supported field `{field}` with a real value must stay compliant"
        );
    }
}

/// Unknown-field control (pre-existing behaviour, pinned here so the new
/// contrasts cannot silently loosen it).
#[test]
fn an_unknown_required_field_still_fails_closed() {
    let event = fully_evidenced_event().build();
    assert!(insufficient(
        &event.local_verdict(&require("a_field_we_do_not_emit"))
    ));
}
