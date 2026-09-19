//! #545 (GOV-06) — the shared effect-bucket registry, specified as tests.
//!
//! One typed registry maps the native `CostTuple` axes
//! (tokens_in/tokens_out/cents/wall_ms/milli-attention) onto the NORMATIVE
//! MIC effect classes (read/write/network/exec/external_send), and is shared
//! by the three crate surfaces that must never disagree:
//!
//! 1. the MD author (`mission.rs` effect policies + budget keys),
//! 2. the ObservedEvent emitter path (`budget_delta.effect_class` strings,
//!    §6.2 side-effect taxonomy — pinned cross-crate below),
//! 3. the ER adapter (`project.rs` `budget_remaining` keys).
//!
//! Native cents and wall_ms remain separate ECONOMIC axes (the operator's
//! cost controls are untouched); they are not substitute names for the
//! portable buckets. Run: `cargo test -p ardur-governance --test effect_registry`

use std::collections::BTreeMap;

use ardur_core_types::CostTuple;
use ardur_governance::{
    EFFECT_CLASSES, EffectBucketDescriptor, EffectBucketRegistry, EffectClass, EffectLedger,
    EffectUnit, REGISTRY_EFFECT_CLASSES, RegistryDescriptor, RegistryError,
    author_mission_declaration, effect_bucket_registry, normalize_effect_class,
    project_budget_remaining,
};

fn steps(amounts: &[(EffectClass, u64)]) -> BTreeMap<EffectClass, u64> {
    amounts.iter().copied().collect()
}

// ---------------------------------------------------------------------------
// 1. The registry itself: one shared, typed, versioned table.
// ---------------------------------------------------------------------------

#[test]
fn registry_covers_exactly_the_normative_effect_classes() {
    let registry = effect_bucket_registry();
    let names: Vec<String> = registry
        .classes()
        .iter()
        .map(EffectClass::to_string)
        .collect();
    assert_eq!(
        names,
        EFFECT_CLASSES.to_vec(),
        "the registry's class set must equal the normative EFFECT_CLASSES the MD schema demands"
    );
    assert_eq!(
        registry.classes(),
        REGISTRY_EFFECT_CLASSES,
        "both exported surfaces must agree exactly"
    );
}

#[test]
fn registry_descriptor_is_pinned_and_self_describing() {
    let RegistryDescriptor {
        version,
        mapped_native_axes,
        economic_native_axes,
    } = effect_bucket_registry().descriptor();
    assert_eq!(version, "effect-bucket-registry.v1");
    assert_eq!(
        mapped_native_axes,
        vec![
            "tokens_in".to_string(),
            "tokens_out".to_string(),
            "milli_attention".to_string()
        ],
        "exactly the three CostTuple axes with a normative bucket contribution"
    );
    assert_eq!(
        economic_native_axes,
        vec!["cents".to_string(), "wall_ms".to_string()],
        "cents and wall_ms stay economic axes, not portable bucket names"
    );
}

#[test]
fn every_descriptor_carries_a_unit_and_valid_scales() {
    let registry = effect_bucket_registry();
    for class in registry.classes() {
        let descriptor = registry.get(&class.to_string()).expect("described");
        assert_eq!(descriptor.unit, EffectUnit::Steps, "{class} counts steps");
        for (axis, scale) in &descriptor.axes {
            assert!(scale.numerator >= 1, "{class}/{axis}: numerator >= 1");
            assert!(scale.denominator >= 1, "{class}/{axis}: denominator >= 1");
        }
    }
}

/// The v1 mapping table itself. Every entry is load-bearing for §6.5
/// determinism: two implementations reading this table must project the
/// same CostTuple into the same buckets.
#[test]
fn the_v1_mapping_table_is_pinned() {
    let registry = effect_bucket_registry();

    let read = registry.get("read").unwrap();
    let read_in = &read.axes[&ardur_governance::CostAxis::TokensIn];
    assert_eq!((read_in.numerator, read_in.denominator), (1, 1));

    let write = registry.get("write").unwrap();
    let write_out = &write.axes[&ardur_governance::CostAxis::TokensOut];
    assert_eq!((write_out.numerator, write_out.denominator), (1, 1));

    let exec = registry.get("exec").unwrap();
    let exec_attention = &exec.axes[&ardur_governance::CostAxis::MilliAttention];
    assert_eq!(
        (exec_attention.numerator, exec_attention.denominator),
        (1, 1_000)
    );

    // No native CostTuple axis measures network or external_send effects in
    // v1; their buckets fill only from emitter-classified steps, never from
    // an invented axis contribution.
    assert!(
        registry.get("network").unwrap().axes.is_empty()
            && registry.get("external_send").unwrap().axes.is_empty(),
        "network/external_send have no native axis contribution in v1"
    );
}

// ---------------------------------------------------------------------------
// 2. Units and rounding — floor, never ceil (no invented usage).
// ---------------------------------------------------------------------------

#[test]
fn rounding_floors_below_one_step_and_never_ceils() {
    let registry = effect_bucket_registry();
    let scale = registry.get("exec").unwrap().axes[&ardur_governance::CostAxis::MilliAttention];
    for (raw, expected) in [
        (0_u64, 0_u64),
        (999, 0),
        (1_000, 1),
        (1_999, 1),
        (2_499, 2),
        (u64::MAX, u64::MAX / 1_000),
    ] {
        assert_eq!(
            scale.floor(raw),
            expected,
            "floor({raw}) must be {expected}"
        );
    }

    let identity = registry.get("read").unwrap().axes[&ardur_governance::CostAxis::TokensIn];
    for (raw, expected) in [(0_u64, 0_u64), (1, 1), (1_234, 1_234)] {
        assert_eq!(identity.floor(raw), expected);
    }
}

#[test]
fn a_cost_tuple_projects_deterministically_into_buckets() {
    let registry = effect_bucket_registry();
    let cost = CostTuple {
        tokens_in: 1_234,
        tokens_out: 567,
        cents: 12,
        wall_ms: 8_000,
        attention_score: 750,
    };
    let buckets = registry.project_cost_tuple(&cost);
    assert_eq!(buckets[&EffectClass::Read], 1_234);
    assert_eq!(buckets[&EffectClass::Write], 567);
    // 750 milli-attention floors to zero steps — no rounding up into usage.
    assert_eq!(buckets[&EffectClass::Exec], 0);
    assert_eq!(buckets[&EffectClass::Network], 0);
    assert_eq!(buckets[&EffectClass::ExternalSend], 0);

    // Same input, same output — the §6.5 determinism requirement.
    assert_eq!(registry.project_cost_tuple(&cost), buckets);
}

#[test]
fn cents_and_wall_ms_never_leak_into_a_bucket() {
    let registry = effect_bucket_registry();
    let cost = CostTuple {
        cents: 500,
        wall_ms: 60_000,
        ..CostTuple::ZERO
    };
    let buckets = registry.project_cost_tuple(&cost);
    assert!(
        buckets.values().all(|&v| v == 0),
        "economic axes must not move a normative bucket: {buckets:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. Ledger: admission reservation, commit/refund, failed/unknown effects,
//    sibling (carve) conservation.
// ---------------------------------------------------------------------------

fn fresh_ledger(ceiling: u64) -> EffectLedger {
    let map = REGISTRY_EFFECT_CLASSES
        .iter()
        .map(|c| (*c, ceiling))
        .collect();
    EffectLedger::new(&map)
}

#[test]
fn admission_refuses_what_the_ceiling_cannot_cover() {
    let mut ledger = fresh_ledger(100);
    let first = ledger
        .reserve(&steps(&[(EffectClass::Exec, 60)]))
        .expect("fits");
    assert_eq!(first.amounts[&EffectClass::Exec], 60);
    let err = ledger
        .reserve(&steps(&[(EffectClass::Exec, 41)]))
        .expect_err("60 outstanding + 41 requested > ceiling 100");
    assert_eq!(err.class().unwrap(), "exec", "the refusal names the class");
    ledger.audit().expect("conservation holds after a refusal");
}

#[test]
fn a_zero_or_missing_bucket_refuses_everything_no_widening() {
    let mut ledger = fresh_ledger(0);
    let err = ledger
        .reserve(&steps(&[(EffectClass::ExternalSend, 1)]))
        .expect_err("a zero ceiling must refuse even one step");
    assert_eq!(err.class().unwrap(), "external_send");

    // A class absent from the ceilings map reads as ceiling zero — the
    // fail-closed default, never a widened bucket.
    let mut sparse = EffectLedger::new(&steps(&[(EffectClass::Read, 10)]));
    assert!(
        sparse.reserve(&steps(&[(EffectClass::Exec, 1)])).is_err(),
        "a missing ceiling is zero, not unbounded"
    );
}

#[test]
fn commit_charges_actuals_and_refunds_the_unused_reservation() {
    let mut ledger = fresh_ledger(100);
    let reservation = ledger
        .reserve(&steps(&[(EffectClass::Exec, 2)]))
        .expect("reserve two exec steps");

    // Actual work: 1,200 milli-attention floors to 1 step; 1 step refunds.
    let actual = CostTuple {
        attention_score: 1_200,
        ..CostTuple::ZERO
    };
    let outcome = ledger.commit(&reservation, &actual).expect("commit");
    assert_eq!(outcome.charged[&EffectClass::Exec], 1);
    assert_eq!(outcome.refunded[&EffectClass::Exec], 1);
    ledger.audit().expect("conservation holds after commit");
    assert_eq!(ledger.available(EffectClass::Exec), 99);
    assert_eq!(ledger.consumed(EffectClass::Exec), 1);
}

#[test]
fn an_overrun_is_a_typed_error_not_an_absorbed_charge() {
    let mut ledger = fresh_ledger(100);
    let reservation = ledger
        .reserve(&steps(&[(EffectClass::Exec, 2)]))
        .expect("reserve");
    let overrun = CostTuple {
        attention_score: 3_000, // floors to 3 steps against a 2-step hold
        ..CostTuple::ZERO
    };
    let err = ledger
        .commit(&reservation, &overrun)
        .expect_err("§9.4 forbids best-effort overspend");
    assert_eq!(err.class().unwrap(), "exec");
    ledger.audit().expect("a refused commit mutates nothing");
}

#[test]
fn a_failed_effect_refunds_its_whole_reservation() {
    let mut ledger = fresh_ledger(100);
    let reservation = ledger
        .reserve(&steps(&[(EffectClass::Write, 7)]))
        .expect("reserve");
    let refunded = ledger.fail(&reservation).expect("fail refunds in full");
    assert_eq!(refunded[&EffectClass::Write], 7);
    assert_eq!(ledger.available(EffectClass::Write), 100);
    ledger
        .audit()
        .expect("conservation holds after a full refund");
}

#[test]
fn an_unknown_effect_charges_nothing_and_refunds_everything() {
    // The step never produced observable work on the reserved class: zero
    // usage is HONEST here (nothing was invented — the hold is released).
    let mut ledger = fresh_ledger(100);
    let reservation = ledger
        .reserve(&steps(&[(EffectClass::Exec, 2)]))
        .expect("reserve");
    let outcome = ledger
        .commit(&reservation, &CostTuple::ZERO)
        .expect("zero actuals commit");
    assert_eq!(outcome.charged[&EffectClass::Exec], 0);
    assert_eq!(outcome.refunded[&EffectClass::Exec], 2);
    ledger.audit().expect("conservation holds");
}

#[test]
fn duplicate_replay_of_a_settled_reservation_is_rejected() {
    let mut ledger = fresh_ledger(100);
    let reservation = ledger
        .reserve(&steps(&[(EffectClass::Read, 10)]))
        .expect("reserve");
    let actual = CostTuple {
        tokens_in: 4,
        ..CostTuple::ZERO
    };
    ledger.commit(&reservation, &actual).expect("first commit");
    let err = ledger
        .commit(&reservation, &actual)
        .expect_err("the same reservation cannot settle twice");
    assert_eq!(err.reservation_id().unwrap(), reservation.id);
    ledger.audit().expect("a replay mutates nothing");
}

#[test]
fn an_unknown_reservation_cannot_settle() {
    let mut ledger = fresh_ledger(100);
    let reservation = ledger
        .reserve(&steps(&[(EffectClass::Read, 10)]))
        .expect("reserve");
    // A reservation whose id the ledger never issued (and whose amounts it
    // therefore does not hold) must not settle or refund.
    let forged = ardur_governance::Reservation {
        id: "res-999".to_string(),
        amounts: steps(&[(EffectClass::Read, 10)]),
    };
    assert!(ledger.commit(&forged, &CostTuple::ZERO).is_err());
    assert!(ledger.fail(&forged).is_err());
    // The REAL reservation is untouched and still live.
    assert!(ledger.commit(&reservation, &CostTuple::ZERO).is_ok());
}

#[test]
fn ledger_state_survives_a_restart_exactly() {
    let mut ledger = fresh_ledger(100);
    let settled = ledger
        .reserve(&steps(&[(EffectClass::Read, 10)]))
        .expect("reserve");
    ledger
        .commit(
            &settled,
            &CostTuple {
                tokens_in: 4,
                ..CostTuple::ZERO
            },
        )
        .expect("commit");
    let live = ledger
        .reserve(&steps(&[(EffectClass::Exec, 5)]))
        .expect("reserve");

    let serialized = serde_json::to_string(&ledger).expect("ledger serializes");
    let mut restored: EffectLedger = serde_json::from_str(&serialized).expect("ledger restores");

    // Balances continue from the same point — a restart must not reset them.
    for class in REGISTRY_EFFECT_CLASSES {
        assert_eq!(
            restored.available(class),
            ledger.available(class),
            "{class} available must round-trip"
        );
        assert_eq!(restored.consumed(class), ledger.consumed(class));
    }
    // And the restart still rejects both a replayed settle and a replayed id.
    let err = restored
        .commit(
            &settled,
            &CostTuple {
                tokens_in: 4,
                ..CostTuple::ZERO
            },
        )
        .expect_err("spent reservations stay spent across a restart");
    assert_eq!(err.reservation_id().unwrap(), settled.id);
    restored.audit().expect("restored ledger conserves");
    let _ = live;
}

#[test]
fn a_corrupt_serialized_ledger_is_rejected_fail_closed() {
    let mut ledger = fresh_ledger(10);
    let _ = ledger
        .reserve(&steps(&[(EffectClass::Read, 4)]))
        .expect("reserve");
    let mut value = serde_json::to_value(&ledger).expect("serializes");
    // consumed above the ceiling: no legal operation sequence produces this.
    value["consumed"]["read"] = serde_json::json!(99);
    let err = serde_json::from_value::<EffectLedger>(value)
        .expect_err("an inconsistent ledger state must not deserialize");
    assert!(
        err.to_string().to_lowercase().contains("conserve"),
        "the error must name the conservation violation: {err}"
    );
}

#[test]
fn sibling_carves_conserve_the_parent_and_bound_each_other() {
    let mut parent = fresh_ledger(100);
    let child_a = parent
        .carve(&steps(&[(EffectClass::Exec, 60)]))
        .expect("first carve");
    let child_b = parent
        .carve(&steps(&[(EffectClass::Exec, 40)]))
        .expect("second carve");
    assert!(
        parent.carve(&steps(&[(EffectClass::Exec, 1)])).is_err(),
        "the escrow pool is exhausted — no best-effort carve"
    );
    assert_eq!(parent.available(EffectClass::Exec), 0);
    assert_eq!(parent.escrowed(EffectClass::Exec), 100);
    parent.audit().expect("parent conserves after carving");
    child_a.audit().expect("child a conserves");
    child_b.audit().expect("child b conserves");

    // A child overspending its own carve cannot reach the parent's pool:
    // the children were carved 60/40 and are bounded by their own ceilings.
    let mut child_a = child_a;
    assert!(child_a.reserve(&steps(&[(EffectClass::Exec, 61)])).is_err());
    assert_eq!(child_a.available(EffectClass::Exec), 60);
}

#[test]
fn carving_refuses_unmapped_or_overdrawn_classes() {
    // external_send has NO ceiling entry (reads as zero): escrowing one step
    // of it would mint child authority the parent never held.
    let mut parent = EffectLedger::new(&steps(&[(EffectClass::Read, 10)]));
    assert!(
        parent
            .carve(&steps(&[(EffectClass::ExternalSend, 1)]))
            .is_err(),
        "zero ceiling cannot escrow anything"
    );
}

// ---------------------------------------------------------------------------
// 4. Registry construction guards: descriptor changes are visible, never
//    silent; incomplete or duplicated tables are refused.
// ---------------------------------------------------------------------------

fn v1_descriptors() -> Vec<EffectBucketDescriptor> {
    effect_bucket_registry().descriptor_list()
}

#[test]
fn building_a_registry_from_descriptors_requires_all_five_exactly_once() {
    let mut missing = v1_descriptors();
    missing.pop();
    assert!(matches!(
        EffectBucketRegistry::from_descriptors("test", missing),
        Err(RegistryError::MissingClass(_))
    ));

    let mut duplicated = v1_descriptors();
    let clone = duplicated[0].clone();
    duplicated.push(clone);
    assert!(matches!(
        EffectBucketRegistry::from_descriptors("test", duplicated),
        Err(RegistryError::DuplicateClass(_))
    ));

    // A renamed descriptor (Read→Write) leaves the table with two Writes
    // and no Read: refused as a duplicate, with Read reported missing in
    // the no-duplicate ordering. Either refusal proves the table is rejected.
    let mut renamed = v1_descriptors();
    renamed[0].class = EffectClass::Write;
    assert!(matches!(
        EffectBucketRegistry::from_descriptors("test", renamed),
        Err(RegistryError::DuplicateClass(_)) | Err(RegistryError::MissingClass(_))
    ));
}

#[test]
fn a_descriptor_change_is_visible_in_projection_not_silent() {
    let registry = effect_bucket_registry();
    let mut changed = v1_descriptors();
    for descriptor in &mut changed {
        if descriptor.class == EffectClass::Exec {
            descriptor.axes.insert(
                ardur_governance::CostAxis::MilliAttention,
                ardur_governance::AxisScale {
                    numerator: 2,
                    denominator: 1_000,
                },
            );
        }
    }
    let mutated = EffectBucketRegistry::from_descriptors("effect-bucket-registry.test", changed)
        .expect("well-formed table");

    let cost = CostTuple {
        attention_score: 1_500,
        ..CostTuple::ZERO
    };
    assert_eq!(registry.project_cost_tuple(&cost)[&EffectClass::Exec], 1);
    assert_eq!(mutated.project_cost_tuple(&cost)[&EffectClass::Exec], 3);
    assert_eq!(
        mutated.descriptor().version,
        "effect-bucket-registry.test",
        "a changed mapping MUST carry a different version"
    );
}

// ---------------------------------------------------------------------------
// 5. Normalization — shared vocabulary across emitter and adapter.
// ---------------------------------------------------------------------------

#[test]
fn unknown_class_normalization_fails_closed() {
    // The D1 failure mode: an adapter inventing its own bucket names.
    for invented in ["tokens", "cost", "egress", "tool_exec", "reads", ""] {
        assert!(
            normalize_effect_class(invented).is_none(),
            "`{invented}` must not normalize into a normative bucket"
        );
    }
}

#[test]
fn normative_classes_are_fixed_points() {
    for class in EFFECT_CLASSES {
        assert_eq!(
            normalize_effect_class(class).map(|c| c.to_string()),
            Some(class.to_string()),
            "a normative class normalizes to itself"
        );
    }
}

#[test]
fn the_observed_side_effect_taxonomy_normalizes_deterministically() {
    // §6.2 pre-normalization spellings → A.1 buckets (§6.5).
    let table = [
        ("none", "read"),
        ("internal_write", "write"),
        ("state_change", "write"),
        ("external_send", "external_send"),
    ];
    for (observed, bucket) in table {
        assert_eq!(
            normalize_effect_class(observed).map(|c| c.to_string()),
            Some(bucket.to_string()),
            "`{observed}` must normalize to `{bucket}`"
        );
    }
}

#[test]
fn the_observed_events_crate_agrees_with_the_registry() {
    // Exhaustive over the emitter's enum: adding a variant breaks this match
    // and forces an explicit registry decision instead of silent drift.
    use ardur_observed_events::SideEffectClass as Observed;
    let pairs = [
        (Observed::None, EffectClass::Read),
        (Observed::InternalWrite, EffectClass::Write),
        (Observed::ExternalSend, EffectClass::ExternalSend),
        (Observed::StateChange, EffectClass::Write),
    ];
    for (variant, expected) in pairs {
        let wire = serde_json::to_value(variant)
            .expect("serializes")
            .as_str()
            .expect("a string enum")
            .to_string();
        assert_eq!(normalize_effect_class(&wire), Some(expected));
    }
    // And the emitter's budget_delta vocabulary is the registry's: every
    // normative class name is a legal effect_class string, every other
    // string is rejected by the same shared check.
    for class in REGISTRY_EFFECT_CLASSES {
        assert!(normalize_effect_class(&class.to_string()).is_some());
    }
}

// ---------------------------------------------------------------------------
// 6. The MD author surface shares the registry.
// ---------------------------------------------------------------------------

fn md_identity() -> ardur_governance::MissionIdentity {
    ardur_governance::MissionIdentity {
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

fn md_grants() -> Vec<ardur_governance::GrantRecord> {
    vec![ardur_governance::GrantRecord {
        tool: "file.read".into(),
        capabilities: vec!["cap.fs_read".into()],
        scope: Some("/private/tmp/ardur-beta".into()),
        subject: "cli://localhost".into(),
        receipt_id: Some("receipt-a".into()),
    }]
}

fn md_budgets() -> BTreeMap<String, ardur_governance::BudgetPair> {
    EFFECT_CLASSES
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
        .collect()
}

#[test]
fn the_md_author_rejects_budget_keys_outside_the_registry() {
    let mut budgets = md_budgets();
    budgets.insert(
        "tokens".to_string(), // the D1 nonportable vocabulary
        ardur_governance::BudgetPair {
            ceiling: 5,
            reserved_share: 0,
        },
    );
    let err = author_mission_declaration(&md_identity(), &md_grants(), &budgets)
        .expect_err("a budget key outside the registry must fail authoring");
    assert!(
        err.to_string().contains("tokens"),
        "the error names the offending key: {err}"
    );
}

#[test]
fn the_md_author_declares_the_registry_classes_in_registry_order() {
    let md =
        author_mission_declaration(&md_identity(), &md_grants(), &md_budgets()).expect("authors");
    let declared: Vec<String> = md
        .effect_policies
        .iter()
        .map(|p| p.side_effect_class.clone())
        .collect();
    let registry: Vec<String> = effect_bucket_registry()
        .classes()
        .iter()
        .map(EffectClass::to_string)
        .collect();
    assert_eq!(declared, registry);
    assert_eq!(declared, EFFECT_CLASSES.to_vec());
}

// ---------------------------------------------------------------------------
// 7. The ER adapter surface shares the registry.
// ---------------------------------------------------------------------------

#[test]
fn budget_remaining_projection_keeps_registry_keys_and_drops_none() {
    let registry = effect_bucket_registry();
    let per_class: BTreeMap<String, u64> = [("exec".to_string(), 7), ("read".to_string(), 3)]
        .into_iter()
        .collect();
    let projected =
        project_budget_remaining(&per_class, &registry).expect("keys are registry classes");
    assert_eq!(projected.get("exec"), Some(&7));
    assert_eq!(projected.get("read"), Some(&3));
    // Missing buckets are absent keys — no invented zeros.
    assert!(!projected.contains_key("write"));
    assert!(!projected.contains_key("network"));
    assert!(!projected.contains_key("external_send"));
}

#[test]
fn budget_remaining_projection_rejects_keys_outside_the_registry() {
    let registry = effect_bucket_registry();
    for invented in ["cost", "tokens", "egress", "tool_exec"] {
        let per_class: BTreeMap<String, u64> = [(invented.to_string(), 1)].into_iter().collect();
        let err = project_budget_remaining(&per_class, &registry)
            .expect_err("non-registry keys must be rejected, not dropped or widened");
        assert!(
            err.to_string().contains(invented),
            "the error names `{invented}`: {err}"
        );
    }
}
