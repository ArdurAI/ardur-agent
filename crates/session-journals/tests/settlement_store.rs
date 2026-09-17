#![cfg(any(target_os = "linux", target_os = "macos"))]

use ardur_core_types::{HolderId, Sha256Digest, TokenId};
use ardur_session_journals::{SessionId, UnixTsMillis, settlement::*};
use uuid::Uuid;

fn identity() -> ReceiptIdentity {
    ReceiptIdentity {
        receipt_log: "/trusted/receipts/chain.jsonl".into(),
        signer: Sha256Digest::of(b"public signer identity"),
    }
}

fn turn() -> TurnObligation {
    TurnObligation {
        schema_version: SETTLEMENT_SCHEMA_VERSION,
        turn_id: TurnId(Uuid::new_v4()),
        budget_epoch: BudgetEpoch(Uuid::new_v4()),
        request_session: SessionId::new(),
        journal_owner: Some(SessionId::new()),
        verified_subject: HolderId::from("spiffe://test/caller"),
        budget_holder: HolderId::from("spiffe://test/trusted-budget"),
        cap_token_id: TokenId(Uuid::new_v4()),
        started_at: UnixTsMillis(42),
        rounds: vec![],
        terminal: TurnTerminal::Open,
        cancellation_marker: MarkerProjection::NotRequired,
    }
}

fn round(epoch: BudgetEpoch) -> RoundObligation {
    use ardur_session_journals::CostTuple;
    RoundObligation {
        settlement_id: SettlementId(Uuid::new_v4()),
        ordinal: 0,
        provider_request_id: Uuid::new_v4(),
        provider: "fixture".into(),
        model: "paid".into(),
        request_digest: Sha256Digest::of(b"request"),
        reserved: CostTuple::cents(8),
        provider_evidence: ProviderEvidence::Observed {
            usage: Some(UsageSnapshot {
                input_tokens: 5,
                output_tokens: 3,
            }),
            cost: CostTuple::cents(10),
            provenance: CostProvenance::ReportedCost,
            finished: true,
            interrupted: false,
        },
        tools: vec![ToolEvidence {
            ordinal: 0,
            call_id: "call".into(),
            name: "tool".into(),
            arguments_digest: Sha256Digest::of(b"private args"),
            effect: ToolEffect::Completed {
                output_digest: Sha256Digest::of(b"private output"),
                cost: CostTuple::cents(2),
            },
            output_admission: OutputAdmission::Blocked,
        }],
        known_incurred: CostTuple::cents(12),
        decision: Some(SettlementDecision::Refusal(RefusalClass::OutputBlocked)),
        phase: SettlementPhase::Settled {
            application: DebitApplication {
                epoch,
                requested_debit: CostTuple::cents(12),
                applied_debit: CostTuple::cents(9),
                reserved_credit: CostTuple::ZERO,
                additional_debit: CostTuple::cents(1),
                shortfall: CostTuple::cents(3),
                rollback: RollbackStatus::None,
            },
            receipt: None,
        },
        commit_ordinal: Some(1),
        projection: JournalProjection::NotConfigured,
    }
}

#[test]
fn typed_known_expense_and_applied_debit_roundtrip_without_raw_payloads() {
    let mut turn = turn();
    turn.rounds.push(round(turn.budget_epoch));
    turn.terminal = TurnTerminal::Refused(RefusalClass::OutputBlocked);
    let encoded = EncodedSnapshot::new(1, turn.clone(), &SettlementLimits::default()).unwrap();
    assert_eq!(encoded.turn(), &turn);
    let text = std::str::from_utf8(encoded.bytes()).unwrap();
    assert!(!text.contains("private args") && !text.contains("private output"));
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("store");
    let mut store =
        FileSettlementStore::open(&root, identity(), SettlementLimits::default()).unwrap();
    assert_eq!(store.put_exact(None, &encoded), WriteResolution::Durable(1));
    assert_eq!(
        load_settlement_snapshot(&root, &identity(), &SettlementLimits::default())
            .unwrap()
            .snapshots[0]
            .turn(),
        &turn
    );
}

#[test]
fn bounds_schema_order_and_cost_arithmetic_reject_invalid_evidence() {
    let limits = SettlementLimits::default();
    let mut invalid = turn();
    invalid.schema_version += 1;
    assert!(matches!(
        EncodedSnapshot::new(1, invalid, &limits),
        Err(SettlementStoreError::Invalid(
            SettlementProblem::InvalidSchema
        ))
    ));
    let mut invalid = turn();
    invalid.verified_subject.0 = "x".repeat(limits.max_metadata_bytes + 1);
    assert!(matches!(
        EncodedSnapshot::new(1, invalid, &limits),
        Err(SettlementStoreError::Invalid(SettlementProblem::Bounds))
    ));
    assert!(EncodedSnapshot::new(0, turn(), &limits).is_err());
    let tiny = SettlementLimits {
        max_record_bytes: 64,
        ..limits
    };
    assert!(matches!(
        EncodedSnapshot::new(1, turn(), &tiny),
        Err(SettlementStoreError::Invalid(SettlementProblem::Bounds))
    ));
    let mut valid = turn();
    valid.rounds.push(round(valid.budget_epoch));
    let mut invalid = valid.clone();
    invalid.rounds[0].ordinal = 2;
    assert!(EncodedSnapshot::new(1, invalid, &limits).is_err());
    let mut invalid = valid.clone();
    invalid.rounds[0].known_incurred.cents -= 1;
    assert!(EncodedSnapshot::new(1, invalid, &limits).is_err());
    let mut invalid = valid.clone();
    invalid.rounds[0].tools[0].ordinal = 4;
    assert!(EncodedSnapshot::new(1, invalid, &limits).is_err());
    let mut invalid = valid.clone();
    if let ProviderEvidence::Observed { cost, .. } = &mut invalid.rounds[0].provider_evidence {
        cost.cents = u64::MAX;
    }
    assert!(EncodedSnapshot::new(1, invalid, &limits).is_err());
    let mut invalid = valid.clone();
    if let SettlementPhase::Settled { application, .. } = &mut invalid.rounds[0].phase {
        application.shortfall.cents = 0;
    }
    assert!(EncodedSnapshot::new(1, invalid, &limits).is_err());
    let no_tools = SettlementLimits {
        max_tools_per_round: 0,
        ..limits
    };
    assert!(EncodedSnapshot::new(1, valid.clone(), &no_tools).is_err());
    let no_rounds = SettlementLimits {
        max_rounds: 0,
        ..limits
    };
    assert!(EncodedSnapshot::new(1, valid, &no_rounds).is_err());
}

#[test]
fn compare_and_put_is_idempotent_but_revision_conflicts_fail_closed() {
    for conflict in ["same-revision", "stale-previous", "skipped-revision"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("store");
        let limits = SettlementLimits::default();
        let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
        let initial = EncodedSnapshot::new(1, turn(), &limits).unwrap();
        assert_eq!(store.put_exact(None, &initial), WriteResolution::Durable(1));
        assert_eq!(store.put_exact(None, &initial), WriteResolution::Durable(1));
        let next = EncodedSnapshot::new(2, initial.turn().clone(), &limits).unwrap();
        assert_eq!(store.put_exact(Some(1), &next), WriteResolution::Durable(2));
        assert_eq!(store.put_exact(Some(1), &next), WriteResolution::Durable(2));
        let mut altered = next.turn().clone();
        let (previous, revision) = match conflict {
            "same-revision" => {
                altered.terminal = TurnTerminal::Cancelled;
                (Some(1), 2)
            }
            "stale-previous" => (Some(1), 3),
            _ => (Some(2), 4),
        };
        let attempt = EncodedSnapshot::new(revision, altered, &limits).unwrap();
        assert_eq!(
            store.put_exact(previous, &attempt),
            WriteResolution::Unresolved(SettlementProblem::Conflict),
            "{conflict}"
        );
        assert_eq!(
            std::fs::read(root.join(format!("{}.json", initial.turn().turn_id.0))).unwrap(),
            next.bytes()
        );
        let unrelated = EncodedSnapshot::new(1, turn(), &limits).unwrap();
        assert!(
            matches!(
                store.put_exact(None, &unrelated),
                WriteResolution::Unresolved(_)
            ),
            "conflict must latch health"
        );
    }
}

fn file_inventory(root: &std::path::Path) -> std::collections::BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            assert!(entry.file_type().unwrap().is_file());
            (
                entry.file_name().into_string().unwrap(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

#[test]
fn spec_b1_no_pending_mismatch_quarantines_healthy_and_reopened_writers() {
    let mut violations = Vec::new();
    for reopened in [false, true] {
        for mismatch in ["digest", "revision", "unknown-turn"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap().join("store");
            let limits = SettlementLimits::default();
            let snapshot = EncodedSnapshot::new(1, turn(), &limits).unwrap();
            let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
            assert_eq!(
                store.put_exact(None, &snapshot),
                WriteResolution::Durable(1)
            );
            if reopened {
                drop(store);
                store = FileSettlementStore::open(&root, identity(), limits).unwrap();
            }
            assert_eq!(store.health(), StoreHealth::Healthy);
            let before = file_inventory(&root);
            let (id, revision, digest) = match mismatch {
                "digest" => (snapshot.turn().turn_id, 1, Sha256Digest::of(b"wrong")),
                "revision" => (snapshot.turn().turn_id, 2, snapshot.digest()),
                _ => (TurnId(Uuid::new_v4()), 1, snapshot.digest()),
            };
            let result = store.resolve_exact(id, revision, digest);
            let health = store.health();
            let unrelated = EncodedSnapshot::new(1, turn(), &limits).unwrap();
            let unrelated_result = store.put_exact(None, &unrelated);
            // No retained pending bytes exist: even a valid old acknowledgement
            // cannot reset this quarantine or turn a new identity into a retry.
            let valid_lookup = store.resolve_exact(snapshot.turn().turn_id, 1, snapshot.digest());
            let duplicate = store.put_exact(None, &snapshot);
            let closed = WriteResolution::Unresolved(SettlementProblem::Conflict);
            if result != closed
                || health != StoreHealth::Unhealthy(SettlementProblem::Conflict)
                || unrelated_result != closed
                || valid_lookup != closed
                || duplicate != closed
                || store.health() != StoreHealth::Unhealthy(SettlementProblem::Conflict)
                || file_inventory(&root) != before
            {
                violations.push(format!(
                    "reopened={reopened} {mismatch}: resolution={result:?}, health={health:?}, unrelated={unrelated_result:?}, valid_lookup={valid_lookup:?}, duplicate={duplicate:?}, disk_unchanged={}",
                    file_inventory(&root) == before
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "Unresolved must latch closed:\n{}",
        violations.join("\n")
    );
}

#[test]
fn spec_b1_valid_exact_resolution_preserves_healthy_new_turn_admission() {
    for reopened in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("store");
        let limits = SettlementLimits::default();
        let snapshot = EncodedSnapshot::new(1, turn(), &limits).unwrap();
        let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
        assert_eq!(
            store.put_exact(None, &snapshot),
            WriteResolution::Durable(1)
        );
        if reopened {
            drop(store);
            store = FileSettlementStore::open(&root, identity(), limits).unwrap();
        }
        let before = file_inventory(&root);
        assert_eq!(
            store.resolve_exact(snapshot.turn().turn_id, 1, snapshot.digest()),
            WriteResolution::Durable(1)
        );
        assert_eq!(store.health(), StoreHealth::Healthy);
        assert_eq!(file_inventory(&root), before);
        let unrelated = EncodedSnapshot::new(1, turn(), &limits).unwrap();
        assert_eq!(
            store.put_exact(None, &unrelated),
            WriteResolution::Durable(1)
        );
        assert_eq!(store.health(), StoreHealth::Healthy);
        let inventory = load_settlement_snapshot(&root, &identity(), &limits).unwrap();
        assert_eq!(inventory.snapshots.len(), 2);
        for expected in [&snapshot, &unrelated] {
            assert!(
                inventory
                    .snapshots
                    .iter()
                    .any(|stored| stored.bytes() == expected.bytes())
            );
        }
    }
}

fn observed_turn() -> TurnObligation {
    let mut t = turn();
    let mut r = round(t.budget_epoch);
    r.phase = SettlementPhase::WorkObserved;
    r.decision = None;
    t.rounds.push(r);
    t
}

fn assert_evidence_transition(before: TurnObligation, after: TurnObligation, accepted: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("store");
    let limits = SettlementLimits::default();
    let first = EncodedSnapshot::new(1, before, &limits).unwrap();
    let next = EncodedSnapshot::new(2, after, &limits).unwrap();
    let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
    assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
    let retained = file_inventory(&root);
    let result = store.put_exact(Some(1), &next);
    if accepted {
        assert_eq!(result, WriteResolution::Durable(2));
        assert_eq!(store.health(), StoreHealth::Healthy);
        assert_eq!(
            load_settlement_snapshot(&root, &identity(), &limits)
                .unwrap()
                .snapshots[0]
                .bytes(),
            next.bytes()
        );
    } else {
        assert_eq!(
            result,
            WriteResolution::Unresolved(SettlementProblem::InvalidTransition),
            "component evidence must not regress despite an equal aggregate"
        );
        assert_eq!(
            store.health(),
            StoreHealth::Unhealthy(SettlementProblem::InvalidTransition)
        );
        assert_eq!(file_inventory(&root), retained);
        assert_eq!(
            load_settlement_snapshot(&root, &identity(), &limits)
                .unwrap()
                .snapshots[0]
                .bytes(),
            first.bytes()
        );
        let unrelated = EncodedSnapshot::new(1, turn(), &limits).unwrap();
        assert_eq!(
            store.put_exact(None, &unrelated),
            WriteResolution::Unresolved(SettlementProblem::InvalidTransition)
        );
        assert_eq!(file_inventory(&root), retained);
    }
}

fn append_known_tool(t: &mut TurnObligation, cost: ardur_session_journals::CostTuple) {
    let mut tool = t.rounds[0].tools[0].clone();
    tool.ordinal = t.rounds[0].tools.len() as u32;
    tool.effect = ToolEffect::Completed {
        output_digest: Sha256Digest::of(b"second output"),
        cost,
    };
    t.rounds[0].tools.push(tool);
}

#[test]
fn quality_q2_provider_cost_cannot_be_replaced_by_new_tool_at_equal_total() {
    use ardur_session_journals::CostTuple;
    let before = observed_turn();
    let mut after = before.clone();
    after.rounds[0].provider_evidence = ProviderEvidence::NotDispatched;
    append_known_tool(&mut after, CostTuple::cents(10));
    assert_eq!(before.rounds[0].known_incurred, CostTuple::cents(12));
    assert_eq!(after.rounds[0].known_incurred, CostTuple::cents(12));
    assert_evidence_transition(before, after, false);
}

#[test]
fn quality_q2_each_provider_cost_axis_is_independently_cumulative() {
    for axis in 0..5 {
        let mut before = observed_turn();
        if let ProviderEvidence::Observed { cost, .. } = &mut before.rounds[0].provider_evidence {
            *cost = tuple([10; 5]);
        }
        if let ToolEffect::Completed { cost, .. } = &mut before.rounds[0].tools[0].effect {
            *cost = tuple([2; 5]);
        }
        before.rounds[0].known_incurred = tuple([12; 5]);
        let mut after = before.clone();
        let mut decreased = [10; 5];
        decreased[axis] = 9;
        if let ProviderEvidence::Observed { cost, .. } = &mut after.rounds[0].provider_evidence {
            *cost = tuple(decreased);
        }
        let mut compensation = [0; 5];
        compensation[axis] = 1;
        append_known_tool(&mut after, tuple(compensation));
        assert_eq!(
            before.rounds[0].known_incurred,
            after.rounds[0].known_incurred
        );
        assert_evidence_transition(before, after, false);
    }
}

#[test]
fn quality_q2_genuine_provider_growth_and_tool_addition_control() {
    use ardur_session_journals::CostTuple;
    let before = observed_turn();
    let mut after = before.clone();
    append_known_tool(&mut after, CostTuple::cents(10));
    after.rounds[0].known_incurred = CostTuple::cents(22);
    assert_evidence_transition(before, after, true);
    for axis in 0..5 {
        let before = observed_turn();
        let mut after = before.clone();
        let mut increase = [0; 5];
        increase[axis] = 3;
        let increment = tuple(increase);
        if let ProviderEvidence::Observed { cost, usage, .. } =
            &mut after.rounds[0].provider_evidence
        {
            *cost = cost.checked_add(&increment).unwrap();
            *usage = Some(UsageSnapshot {
                input_tokens: 7,
                output_tokens: 8,
            });
        }
        append_known_tool(&mut after, CostTuple::cents(10));
        after.rounds[0].known_incurred = CostTuple::cents(22).checked_add(&increment).unwrap();
        assert_evidence_transition(before, after, true);
    }
}

#[test]
fn quality_q2_zero_cost_provider_observation_cannot_become_not_dispatched() {
    use ardur_session_journals::CostTuple;
    let mut before = observed_turn();
    before.rounds[0].tools.clear();
    before.rounds[0].known_incurred = CostTuple::ZERO;
    before.rounds[0].provider_evidence = ProviderEvidence::Observed {
        usage: Some(UsageSnapshot {
            input_tokens: 5,
            output_tokens: 3,
        }),
        cost: CostTuple::ZERO,
        provenance: CostProvenance::ReportedCost,
        finished: false,
        interrupted: true,
    };
    let mut after = before.clone();
    after.rounds[0].provider_evidence = ProviderEvidence::NotDispatched;
    assert_evidence_transition(before, after, false);
}

#[test]
fn quality_q2_same_cost_provider_facts_cannot_regress() {
    for mutation in [
        "input-usage",
        "output-usage",
        "no-usage",
        "finished",
        "interrupted",
        "provenance",
        "intent",
        "no-dispatch-intent",
    ] {
        let mut before = observed_turn();
        if let ProviderEvidence::Observed { interrupted, .. } =
            &mut before.rounds[0].provider_evidence
        {
            *interrupted = true;
        }
        if mutation == "no-dispatch-intent" {
            before.rounds[0].provider_evidence = ProviderEvidence::DispatchIntent;
            before.rounds[0].known_incurred = ardur_session_journals::CostTuple::cents(2);
        }
        let mut after = before.clone();
        match mutation {
            "intent" => {
                // This case preserves the aggregate with newly observed tool
                // cost so loss of dispatch evidence is independently tested.
                after.rounds[0].provider_evidence = ProviderEvidence::DispatchIntent;
                append_known_tool(&mut after, ardur_session_journals::CostTuple::cents(10));
            }
            "no-dispatch-intent" => {
                after.rounds[0].provider_evidence = ProviderEvidence::NotDispatched
            }
            _ => {
                if let ProviderEvidence::Observed {
                    usage,
                    finished,
                    interrupted,
                    provenance,
                    ..
                } = &mut after.rounds[0].provider_evidence
                {
                    match mutation {
                        "input-usage" => usage.as_mut().unwrap().input_tokens = 4,
                        "output-usage" => usage.as_mut().unwrap().output_tokens = 2,
                        "no-usage" => *usage = None,
                        "finished" => *finished = false,
                        "interrupted" => *interrupted = false,
                        _ => *provenance = CostProvenance::ResponseCost,
                    }
                }
            }
        }
        assert_eq!(
            before.rounds[0].known_incurred,
            after.rounds[0].known_incurred
        );
        assert_evidence_transition(before, after, false);
    }
}

#[test]
fn quality_q2_provider_dispatch_and_late_result_refinement_controls() {
    use ardur_session_journals::CostTuple;
    for initial in [
        ProviderEvidence::NotDispatched,
        ProviderEvidence::DispatchIntent,
        ProviderEvidence::Observed {
            usage: None,
            cost: CostTuple::ZERO,
            provenance: CostProvenance::ReportedCost,
            finished: false,
            interrupted: true,
        },
    ] {
        let mut before = observed_turn();
        before.rounds[0].tools.clear();
        before.rounds[0].known_incurred = CostTuple::ZERO;
        before.rounds[0].provider_evidence = initial;
        let mut after = before.clone();
        after.rounds[0].provider_evidence = ProviderEvidence::Observed {
            usage: Some(UsageSnapshot {
                input_tokens: 5,
                output_tokens: 3,
            }),
            cost: tuple([10; 5]),
            provenance: CostProvenance::ReportedCost,
            finished: true,
            interrupted: true,
        };
        after.rounds[0].known_incurred = tuple([10; 5]);
        assert_evidence_transition(before, after, true);
    }
    for provenance in [
        CostProvenance::ResponseCost,
        CostProvenance::ReportedCost,
        CostProvenance::PricedUsage(Sha256Digest::of(b"rates")),
    ] {
        let mut before = observed_turn();
        if let ProviderEvidence::Observed { provenance: p, .. } =
            &mut before.rounds[0].provider_evidence
        {
            *p = provenance;
        }
        let after = before.clone();
        assert_evidence_transition(before, after, true);
    }
}

fn interrupted_tool_turn() -> TurnObligation {
    let mut t = observed_turn();
    t.rounds[0].provider_evidence = ProviderEvidence::NotDispatched;
    t.rounds[0].known_incurred = ardur_session_journals::CostTuple::ZERO;
    t.rounds[0].tools[0].effect = ToolEffect::InterruptedUnknown;
    t.rounds[0].tools[0].output_admission = OutputAdmission::NotScanned;
    t
}

#[test]
fn quality_q2_interrupted_tool_cannot_become_never_invoked_at_zero_total() {
    let before = interrupted_tool_turn();
    let mut after = before.clone();
    after.rounds[0].tools[0].effect = ToolEffect::NotInvoked(RefusalClass::Authorization);
    assert_eq!(
        before.rounds[0].known_incurred,
        after.rounds[0].known_incurred
    );
    assert_evidence_transition(before, after, false);
}

#[test]
fn quality_q2_interrupted_tool_cannot_erase_possible_effect() {
    for effect in [
        ToolEffect::DispatchIntent,
        ToolEffect::Failed {
            class: ToolFailureClass::Execution,
            effect_unknown: false,
        },
    ] {
        let before = interrupted_tool_turn();
        let mut after = before.clone();
        after.rounds[0].tools[0].effect = effect;
        assert_evidence_transition(before, after, false);
    }
}

#[test]
fn quality_q2_verified_late_tool_results_and_frozen_outcomes_controls() {
    for effect in [
        ToolEffect::Completed {
            output_digest: Sha256Digest::of(b"late result"),
            cost: tuple([3; 5]),
        },
        ToolEffect::Completed {
            output_digest: Sha256Digest::of(b"late free result"),
            cost: tuple([0; 5]),
        },
        ToolEffect::Failed {
            class: ToolFailureClass::Execution,
            effect_unknown: true,
        },
        ToolEffect::InterruptedUnknown,
    ] {
        let before = interrupted_tool_turn();
        let mut after = before.clone();
        if let ToolEffect::Completed { cost, .. } = &effect {
            after.rounds[0].known_incurred = *cost;
            after.rounds[0].tools[0].output_admission = OutputAdmission::Allowed;
        }
        after.rounds[0].tools[0].effect = effect;
        assert_evidence_transition(before, after, true);
    }
    for frozen in [
        ToolEffect::NotInvoked(RefusalClass::Authorization),
        ToolEffect::Failed {
            class: ToolFailureClass::Execution,
            effect_unknown: true,
        },
        ToolEffect::Completed {
            output_digest: Sha256Digest::of(b"free result"),
            cost: tuple([0; 5]),
        },
    ] {
        let mut before = interrupted_tool_turn();
        before.rounds[0].tools[0].effect = frozen;
        let mut after = before.clone();
        after.rounds[0].tools[0].effect = ToolEffect::DispatchIntent;
        assert_evidence_transition(before, after, false);
    }
}

#[test]
fn stable_attribution_and_settled_economics_cannot_be_rewritten() {
    for mutation in [
        "holder",
        "epoch",
        "journal",
        "round-id",
        "expense",
        "erase-round",
        "projection-key",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("store");
        let limits = SettlementLimits::default();
        let mut initial = turn();
        let key = Uuid::new_v4();
        let mut settled = round(initial.budget_epoch);
        settled.projection = JournalProjection::Pending(key);
        initial.rounds.push(settled);
        let first = EncodedSnapshot::new(1, initial.clone(), &limits).unwrap();
        let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
        assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
        initial.rounds[0].projection = JournalProjection::Acknowledged(key);
        let second = EncodedSnapshot::new(2, initial.clone(), &limits).unwrap();
        assert_eq!(
            store.put_exact(Some(1), &second),
            WriteResolution::Durable(2)
        );
        match mutation {
            "holder" => initial.budget_holder.0 = "other-holder".into(),
            "epoch" => {
                initial.budget_epoch = BudgetEpoch(Uuid::new_v4());
                if let SettlementPhase::Settled { application, .. } = &mut initial.rounds[0].phase {
                    application.epoch = initial.budget_epoch;
                }
            }
            "journal" => initial.journal_owner = Some(SessionId::new()),
            "round-id" => initial.rounds[0].settlement_id = SettlementId(Uuid::new_v4()),
            "expense" => {
                initial.rounds[0].known_incurred.cents += 1;
                if let ProviderEvidence::Observed { cost, .. } =
                    &mut initial.rounds[0].provider_evidence
                {
                    cost.cents += 1;
                }
            }
            "erase-round" => initial.rounds.clear(),
            _ => initial.rounds[0].projection = JournalProjection::Acknowledged(Uuid::new_v4()),
        }
        let next = EncodedSnapshot::new(3, initial, &limits).unwrap();
        assert_eq!(
            store.put_exact(Some(2), &next),
            WriteResolution::Unresolved(SettlementProblem::InvalidTransition),
            "{mutation}"
        );
        assert_eq!(
            load_settlement_snapshot(&root, &identity(), &limits)
                .unwrap()
                .snapshots[0]
                .bytes(),
            second.bytes()
        );
    }
}

#[test]
fn inventory_rejects_corrupt_mismatched_unreadable_and_oversized_records() {
    for damage in [
        "digest",
        "basename",
        "oversized",
        "unreadable",
        "unknown-file",
        "format",
        "missing-format",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("store");
        let limits = SettlementLimits::default();
        let snapshot = EncodedSnapshot::new(1, turn(), &limits).unwrap();
        let path = root.join(format!("{}.json", snapshot.turn().turn_id.0));
        let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
        assert_eq!(
            store.put_exact(None, &snapshot),
            WriteResolution::Durable(1)
        );
        drop(store);
        match damage {
            "digest" => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(snapshot.bytes()).unwrap();
                value["payload_digest"] = Sha256Digest::of(b"wrong").to_hex().into();
                std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "basename" => {
                std::fs::rename(&path, root.join(format!("{}.json", Uuid::new_v4()))).unwrap();
            }
            "oversized" => {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(limits.max_record_bytes as u64 + 1)
                    .unwrap();
            }
            "unreadable" => {
                std::fs::remove_file(&path).unwrap();
                std::fs::create_dir(&path).unwrap();
            }
            "unknown-file" => {
                std::fs::write(root.join("unknown.json"), snapshot.bytes()).unwrap();
            }
            "format" => {
                std::fs::write(root.join("format.json"), b"{broken").unwrap();
            }
            _ => {
                std::fs::remove_file(root.join("format.json")).unwrap();
            }
        }
        assert!(
            load_settlement_snapshot(&root, &identity(), &limits).is_err(),
            "{damage}: inventory must not report valid coverage"
        );
        assert!(
            FileSettlementStore::open(&root, identity(), limits).is_err(),
            "{damage}: no automatic repair"
        );
        if damage == "missing-format" {
            assert!(!root.join("format.json").exists());
        }
    }
}

#[test]
fn acknowledged_disappearance_or_rollback_blocks_even_unrelated_writes() {
    for damage in ["delete", "rollback", "lease-replaced", "root-replaced"] {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let root = parent.join("store");
        let limits = SettlementLimits::default();
        let first = EncodedSnapshot::new(1, turn(), &limits).unwrap();
        let second = EncodedSnapshot::new(2, first.turn().clone(), &limits).unwrap();
        let path = root.join(format!("{}.json", first.turn().turn_id.0));
        let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
        assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
        assert_eq!(
            store.put_exact(Some(1), &second),
            WriteResolution::Durable(2)
        );
        match damage {
            "delete" => std::fs::remove_file(&path).unwrap(),
            "rollback" => std::fs::write(&path, first.bytes()).unwrap(),
            "lease-replaced" => {
                std::fs::remove_file(root.join("writer.lock")).unwrap();
                std::fs::write(root.join("writer.lock"), b"").unwrap();
            }
            _ => {
                std::fs::rename(&root, parent.join("old-store")).unwrap();
                drop(FileSettlementStore::open(&root, identity(), limits).unwrap());
            }
        }
        let unrelated = EncodedSnapshot::new(1, turn(), &limits).unwrap();
        let expected = match damage {
            "delete" => SettlementProblem::MissingAcknowledged,
            "rollback" => SettlementProblem::Corrupt,
            _ => SettlementProblem::IdentityChanged,
        };
        assert_eq!(
            store.put_exact(None, &unrelated),
            WriteResolution::Unresolved(expected),
            "{damage}"
        );
        assert_eq!(store.health(), StoreHealth::Unhealthy(expected));
        assert!(
            !root
                .join(format!("{}.json", unrelated.turn().turn_id.0))
                .exists()
        );
        assert!(matches!(
            store.put_exact(None, &first),
            WriteResolution::Unresolved(_)
        ));
    }
}

#[test]
fn prepared_and_finalized_evidence_cannot_be_forgotten_or_reidentified() {
    for mutation in ["candidate", "application", "completed-tool", "decision"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("store");
        let limits = SettlementLimits::default();
        let mut initial = turn();
        let mut r = round(initial.budget_epoch);
        let app = match r.phase.clone() {
            SettlementPhase::Settled { application, .. } => application,
            _ => unreachable!(),
        };
        let candidate = ReceiptCandidate {
            receipt_id: ardur_session_journals::ReceiptId::new(),
            expected_parent: None,
            expected_log_end: 0,
            jws_compact: "e30.e30.c2ln".into(),
            jws_digest: Sha256Digest::of(b"e30.e30.c2ln"),
        };
        r.decision = Some(SettlementDecision::Completion {
            final_answer: false,
        });
        r.phase = SettlementPhase::Finalized {
            application: app,
            candidate: Some(candidate),
        };
        initial.rounds.push(r);
        let first = EncodedSnapshot::new(1, initial.clone(), &limits).unwrap();
        let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
        assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
        match mutation {
            "candidate" => {
                if let SettlementPhase::Finalized {
                    candidate: Some(c), ..
                } = &mut initial.rounds[0].phase
                {
                    c.receipt_id = ardur_session_journals::ReceiptId::new();
                }
            }
            "application" => {
                initial.rounds[0].phase = SettlementPhase::Prepared { candidate: None }
            }
            "completed-tool" => {
                initial.rounds[0].tools[0].effect = ToolEffect::InterruptedUnknown;
                initial.rounds[0].tools[0].output_admission = OutputAdmission::NotScanned;
                initial.rounds[0].known_incurred.cents -= 2;
                initial.rounds[0].phase = SettlementPhase::WorkObserved;
            }
            _ => initial.rounds[0].decision = Some(SettlementDecision::Cancelled),
        }
        let next = EncodedSnapshot::new(2, initial, &limits).unwrap();
        assert_eq!(
            store.put_exact(Some(1), &next),
            WriteResolution::Unresolved(SettlementProblem::InvalidTransition),
            "{mutation}"
        );
    }
}

#[test]
fn fully_rolled_back_prepared_completion_is_not_a_final_answer() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("store");
    let limits = SettlementLimits::default();
    let mut t = turn();
    let mut r = round(t.budget_epoch);
    let mut app = match r.phase {
        SettlementPhase::Settled { application, .. } => application,
        _ => unreachable!(),
    };
    let candidate = ReceiptCandidate {
        receipt_id: ardur_session_journals::ReceiptId::new(),
        expected_parent: None,
        expected_log_end: 0,
        jws_compact: "e30.e30.c2ln".into(),
        jws_digest: Sha256Digest::of(b"e30.e30.c2ln"),
    };
    let receipt_id = candidate.receipt_id;
    r.decision = Some(SettlementDecision::Completion { final_answer: true });
    r.phase = SettlementPhase::Finalized {
        application: app.clone(),
        candidate: Some(candidate),
    };
    t.rounds.push(r);
    let first = EncodedSnapshot::new(1, t.clone(), &limits).unwrap();
    let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
    assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
    app.rollback = RollbackStatus::Applied(app.applied_debit);
    t.rounds[0].phase = SettlementPhase::Settled {
        application: app,
        receipt: None,
    };
    t.terminal = TurnTerminal::Failed(InfrastructureFailureClass::Storage);
    let second = EncodedSnapshot::new(2, t.clone(), &limits)
        .expect("exact rollback can settle without a completion receipt");
    assert_eq!(
        store.put_exact(Some(1), &second),
        WriteResolution::Durable(2)
    );
    t.terminal = TurnTerminal::FinalAnswer(receipt_id);
    assert!(
        EncodedSnapshot::new(3, t, &limits).is_err(),
        "prepared JWS is not an appended final answer"
    );
}

#[test]
fn a_round_can_create_then_acknowledge_one_stable_projection_intent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("store");
    let limits = SettlementLimits::default();
    let mut t = turn();
    let settled = round(t.budget_epoch);
    let mut observed = settled.clone();
    observed.phase = SettlementPhase::WorkObserved;
    observed.decision = None;
    observed.commit_ordinal = None;
    observed.projection = JournalProjection::NotRequired;
    t.rounds.push(observed);
    let first = EncodedSnapshot::new(1, t.clone(), &limits).unwrap();
    let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
    assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
    let key = Uuid::new_v4();
    t.rounds[0] = settled;
    t.rounds[0].projection = JournalProjection::Pending(key);
    let next = EncodedSnapshot::new(2, t.clone(), &limits).unwrap();
    assert_eq!(store.put_exact(Some(1), &next), WriteResolution::Durable(2));
    t.rounds[0].projection = JournalProjection::Acknowledged(key);
    let last = EncodedSnapshot::new(3, t, &limits).unwrap();
    assert_eq!(store.put_exact(Some(2), &last), WriteResolution::Durable(3));
}

#[test]
fn quality_q1_insecure_existing_root_is_rejected_without_initialization() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    for mode in [0o777, 0o750, 0o1700] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("store");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(mode)).unwrap();
        let before = std::fs::metadata(&root).unwrap();
        let opened = FileSettlementStore::open(&root, identity(), SettlementLimits::default());
        let empty = file_inventory(&root).is_empty();
        let after = std::fs::metadata(&root).unwrap();
        assert!(
            matches!(
                opened,
                Err(SettlementStoreError::Invalid(
                    SettlementProblem::IdentityChanged
                ))
            ) && empty
                && before.mode() == after.mode()
                && before.uid() == after.uid(),
            "insecure root mode={mode:o}: opened={}, empty={empty}, mode={:o}",
            opened.is_ok(),
            after.mode()
        );
    }
}

#[test]
fn quality_q1_private_root_create_reopen_inventory_and_put_control() {
    use std::os::unix::fs::MetadataExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("store");
    let limits = SettlementLimits::default();
    let first = EncodedSnapshot::new(1, turn(), &limits).unwrap();
    let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
    assert_eq!(std::fs::metadata(&root).unwrap().mode() & 0o7777, 0o700);
    assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
    for name in file_inventory(&root).keys() {
        let metadata = std::fs::metadata(root.join(name)).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert_eq!(metadata.uid(), std::fs::metadata(&root).unwrap().uid());
    }
    drop(store);
    let mut reopened = FileSettlementStore::open(&root, identity(), limits).unwrap();
    assert_eq!(
        load_settlement_snapshot(&root, &identity(), &limits)
            .unwrap()
            .snapshots[0]
            .bytes(),
        first.bytes()
    );
    assert_eq!(
        reopened.resolve_exact(first.turn().turn_id, 1, first.digest()),
        WriteResolution::Durable(1)
    );
    let next = EncodedSnapshot::new(2, first.turn().clone(), &limits).unwrap();
    assert_eq!(
        reopened.put_exact(Some(1), &next),
        WriteResolution::Durable(2)
    );
    assert_eq!(reopened.health(), StoreHealth::Healthy);
}

#[test]
fn quality_q1_retained_and_live_permissions_fail_closed_without_repair() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let mut violations = Vec::new();
    for target in ["root", "snapshot", "writer.lock", "format.json"] {
        for operation in ["reopen", "put", "resolve"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap().join("store");
            let limits = SettlementLimits::default();
            let first = EncodedSnapshot::new(1, turn(), &limits).unwrap();
            let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
            assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
            let path = match target {
                "root" => root.clone(),
                "snapshot" => root.join(format!("{}.json", first.turn().turn_id.0)),
                _ => root.join(target),
            };
            let mode = match target {
                "root" => 0o777,
                "writer.lock" => 0o666,
                _ => 0o644,
            };
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let before = file_inventory(&root);
            let metadata = std::fs::metadata(&path).unwrap();
            let inventory = load_settlement_snapshot(&root, &identity(), &limits);
            let inventory_rejected = matches!(
                inventory,
                Err(SettlementStoreError::Invalid(
                    SettlementProblem::IdentityChanged
                ))
            );
            let rejected = if operation == "reopen" {
                drop(store);
                matches!(
                    FileSettlementStore::open(&root, identity(), limits),
                    Err(SettlementStoreError::Invalid(
                        SettlementProblem::IdentityChanged
                    ))
                )
            } else {
                let result = if operation == "resolve" {
                    store.resolve_exact(first.turn().turn_id, 1, first.digest())
                } else {
                    let next = EncodedSnapshot::new(2, first.turn().clone(), &limits).unwrap();
                    store.put_exact(Some(1), &next)
                };
                let unrelated = EncodedSnapshot::new(1, turn(), &limits).unwrap();
                result == WriteResolution::Unresolved(SettlementProblem::IdentityChanged)
                    && store.health() == StoreHealth::Unhealthy(SettlementProblem::IdentityChanged)
                    && store.put_exact(None, &unrelated)
                        == WriteResolution::Unresolved(SettlementProblem::IdentityChanged)
            };
            let after = std::fs::metadata(&path).unwrap();
            let unchanged = file_inventory(&root) == before
                && after.mode() == metadata.mode()
                && after.uid() == metadata.uid();
            if !inventory_rejected || !rejected || !unchanged {
                violations.push(format!("{target}/{operation}: inventory_rejected={inventory_rejected}, operation_rejected={rejected}, unchanged={unchanged}"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "private descriptors required:\n{}",
        violations.join("\n")
    );
}

#[test]
fn nofollow_controls_preserve_outside_targets_and_private_files() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let temp = tempfile::tempdir().unwrap();
    let parent = temp.path().canonicalize().unwrap();
    let outside = parent.join("outside");
    std::fs::create_dir(&outside).unwrap();
    symlink(&outside, parent.join("link")).unwrap();
    assert!(
        FileSettlementStore::open(
            &parent.join("link/store"),
            identity(),
            SettlementLimits::default()
        )
        .is_err()
    );
    assert!(!outside.join("store").exists());
    assert!(
        FileSettlementStore::open(
            &parent.join("link"),
            identity(),
            SettlementLimits::default()
        )
        .is_err()
    );
    let victim = outside.join("victim");
    std::fs::write(&victim, b"unchanged").unwrap();
    for target in ["snapshot", "writer.lock", "format.json"] {
        let root = parent.join(target);
        let limits = SettlementLimits::default();
        let snapshot = EncodedSnapshot::new(1, turn(), &limits).unwrap();
        let snapshot_name = format!("{}.json", snapshot.turn().turn_id.0);
        let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
        assert_eq!(
            store.put_exact(None, &snapshot),
            WriteResolution::Durable(1)
        );
        for name in [&snapshot_name[..], "writer.lock", "format.json"] {
            assert_eq!(
                std::fs::metadata(root.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let target = if target == "snapshot" {
            &snapshot_name
        } else {
            target
        };
        std::fs::remove_file(root.join(target)).unwrap();
        symlink(&victim, root.join(target)).unwrap();
        assert!(load_settlement_snapshot(&root, &identity(), &limits).is_err());
        assert!(matches!(
            store.put_exact(None, &snapshot),
            WriteResolution::Unresolved(_)
        ));
        drop(store);
        assert!(FileSettlementStore::open(&root, identity(), limits).is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"unchanged");
    }
}

#[test]
fn inventory_limits_refuse_without_pruning_retained_history() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("store");
    let limits = SettlementLimits {
        max_inventory_records: 1,
        ..SettlementLimits::default()
    };
    let one = EncodedSnapshot::new(1, turn(), &limits).unwrap();
    let two = EncodedSnapshot::new(1, turn(), &limits).unwrap();
    let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
    assert_eq!(store.put_exact(None, &one), WriteResolution::Durable(1));
    assert_eq!(
        store.put_exact(None, &two),
        WriteResolution::Unresolved(SettlementProblem::Bounds)
    );
    assert_eq!(
        load_settlement_snapshot(&root, &identity(), &limits)
            .unwrap()
            .snapshots[0]
            .bytes(),
        one.bytes()
    );
    let smaller = SettlementLimits {
        max_inventory_records: 0,
        ..limits
    };
    assert!(matches!(
        load_settlement_snapshot(&root, &identity(), &smaller),
        Err(SettlementStoreError::Invalid(SettlementProblem::Bounds))
    ));
}

#[test]
fn cancellation_keeps_all_axes_and_distinct_tool_outcomes_without_debit() {
    use ardur_session_journals::CostTuple;
    let mut t = turn();
    let mut r = round(t.budget_epoch);
    let known = CostTuple {
        tokens_in: 3,
        tokens_out: 4,
        cents: 5,
        wall_ms: 6,
        attention_score: 7,
    };
    r.reserved = known;
    if let ProviderEvidence::Observed { cost, .. } = &mut r.provider_evidence {
        *cost = known;
    }
    if let ToolEffect::Completed { cost, .. } = &mut r.tools[0].effect {
        *cost = CostTuple::ZERO;
    }
    for (ordinal, effect) in [
        ToolEffect::Failed {
            class: ToolFailureClass::Timeout,
            effect_unknown: true,
        },
        ToolEffect::NotInvoked(RefusalClass::Authorization),
        ToolEffect::InterruptedUnknown,
        ToolEffect::DispatchIntent,
    ]
    .into_iter()
    .enumerate()
    {
        r.tools.push(ToolEvidence {
            ordinal: ordinal as u32 + 1,
            call_id: "call".into(),
            name: "tool".into(),
            arguments_digest: Sha256Digest::of(b"args"),
            effect,
            output_admission: OutputAdmission::NotScanned,
        });
    }
    r.known_incurred = known;
    r.decision = Some(SettlementDecision::Cancelled);
    r.phase = SettlementPhase::Settled {
        application: DebitApplication {
            epoch: t.budget_epoch,
            requested_debit: CostTuple::ZERO,
            applied_debit: CostTuple::ZERO,
            reserved_credit: known,
            additional_debit: CostTuple::ZERO,
            shortfall: CostTuple::ZERO,
            rollback: RollbackStatus::None,
        },
        receipt: None,
    };
    t.rounds.push(r);
    t.terminal = TurnTerminal::Cancelled;
    let limits = SettlementLimits::default();
    let encoded = EncodedSnapshot::new(1, t.clone(), &limits).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("store");
    let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
    assert_eq!(store.put_exact(None, &encoded), WriteResolution::Durable(1));
    drop(store);
    assert_eq!(
        load_settlement_snapshot(&root, &identity(), &limits)
            .unwrap()
            .snapshots[0]
            .turn(),
        &t
    );
}

#[test]
fn receipt_identity_is_bounded_and_absolute_before_root_creation() {
    for path in [
        "relative/chain.jsonl".to_owned(),
        "/trusted/../chain.jsonl".to_owned(),
        format!("/{}", "x".repeat(2048)),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("store");
        let mut binding = identity();
        binding.receipt_log = path.into();
        assert!(matches!(
            FileSettlementStore::open(&root, binding, SettlementLimits::default()),
            Err(SettlementStoreError::Invalid(_))
        ));
        assert!(!root.exists(), "invalid binding must not initialize a root");
    }
}

#[test]
fn cancellation_marker_identity_survives_preparation_and_acknowledgement() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("store");
    let limits = SettlementLimits::default();
    let mut t = turn();
    t.terminal = TurnTerminal::Cancelled;
    let id = ardur_session_journals::ReceiptId::new();
    t.cancellation_marker = MarkerProjection::Pending(id);
    let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
    let first = EncodedSnapshot::new(1, t.clone(), &limits).unwrap();
    assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
    let candidate = ReceiptCandidate {
        receipt_id: id,
        expected_parent: None,
        expected_log_end: 0,
        jws_compact: "e30.e30.c2ln".into(),
        jws_digest: Sha256Digest::of(b"e30.e30.c2ln"),
    };
    t.cancellation_marker = MarkerProjection::Prepared(candidate.clone());
    let second = EncodedSnapshot::new(2, t.clone(), &limits).unwrap();
    assert_eq!(
        store.put_exact(Some(1), &second),
        WriteResolution::Durable(2)
    );
    t.cancellation_marker = MarkerProjection::Acknowledged(ReceiptBinding {
        receipt_id: id,
        jws_digest: candidate.jws_digest,
    });
    let third = EncodedSnapshot::new(3, t.clone(), &limits).unwrap();
    assert_eq!(
        store.put_exact(Some(2), &third),
        WriteResolution::Durable(3)
    );
    t.cancellation_marker = MarkerProjection::Pending(ardur_session_journals::ReceiptId::new());
    let conflicting = EncodedSnapshot::new(4, t, &limits).unwrap();
    assert_eq!(
        store.put_exact(Some(3), &conflicting),
        WriteResolution::Unresolved(SettlementProblem::InvalidTransition)
    );
}

#[test]
fn revision_zero_is_not_an_alias_for_an_absent_previous_revision() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("store");
    let limits = SettlementLimits::default();
    let snapshot = EncodedSnapshot::new(1, turn(), &limits).unwrap();
    let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
    assert_eq!(
        store.put_exact(None, &snapshot),
        WriteResolution::Durable(1)
    );
    assert_eq!(
        store.put_exact(Some(0), &snapshot),
        WriteResolution::Unresolved(SettlementProblem::Conflict)
    );
}

#[test]
fn headroom_clamped_refund_retains_applied_movement_above_requested_expense() {
    use ardur_session_journals::CostTuple;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("store");
    let limits = SettlementLimits::default();
    let mut t = turn();
    let mut r = round(t.budget_epoch);
    r.reserved = CostTuple::cents(100);
    r.phase = SettlementPhase::Unresolved {
        last_definite: DefinitePhase::Finalized,
        problem: SettlementProblem::ReleaseCreditPending,
        application: Some(DebitApplication {
            epoch: t.budget_epoch,
            requested_debit: CostTuple::ZERO,
            applied_debit: CostTuple::cents(80),
            reserved_credit: CostTuple::cents(20),
            additional_debit: CostTuple::ZERO,
            shortfall: CostTuple::ZERO,
            rollback: RollbackStatus::None,
        }),
        candidate: None,
    };
    r.decision = Some(SettlementDecision::Cancelled);
    t.rounds.push(r);
    t.terminal = TurnTerminal::Unresolved(SettlementProblem::ReleaseCreditPending);
    let snapshot = EncodedSnapshot::new(1, t.clone(), &limits)
        .expect("actual debit above requested/known must remain representable");
    let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
    assert_eq!(
        store.put_exact(None, &snapshot),
        WriteResolution::Durable(1)
    );
    drop(store);
    assert_eq!(
        load_settlement_snapshot(&root, &identity(), &limits)
            .unwrap()
            .snapshots[0]
            .turn(),
        &t
    );
}

#[test]
fn pending_reserved_credit_progresses_without_claiming_early_settlement() {
    use ardur_session_journals::CostTuple;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("store");
    let limits = SettlementLimits::default();
    let mut t = turn();
    let mut r = round(t.budget_epoch);
    r.reserved = CostTuple::cents(100);
    r.decision = Some(SettlementDecision::Cancelled);
    let mut app = DebitApplication {
        epoch: t.budget_epoch,
        requested_debit: CostTuple::ZERO,
        applied_debit: CostTuple::cents(80),
        reserved_credit: CostTuple::cents(20),
        additional_debit: CostTuple::ZERO,
        shortfall: CostTuple::ZERO,
        rollback: RollbackStatus::None,
    };
    r.phase = SettlementPhase::Unresolved {
        last_definite: DefinitePhase::Finalized,
        problem: SettlementProblem::ReleaseCreditPending,
        application: Some(app.clone()),
        candidate: None,
    };
    t.rounds.push(r);
    t.terminal = TurnTerminal::Unresolved(SettlementProblem::ReleaseCreditPending);
    let first = EncodedSnapshot::new(1, t.clone(), &limits).unwrap();
    let mut premature = t.clone();
    premature.rounds[0].phase = SettlementPhase::Settled {
        application: app.clone(),
        receipt: None,
    };
    premature.terminal = TurnTerminal::Cancelled;
    assert!(
        EncodedSnapshot::new(2, premature, &limits).is_err(),
        "pending refund cannot become settled cancellation"
    );
    let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
    assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
    app.reserved_credit = CostTuple::cents(70);
    app.applied_debit = CostTuple::cents(30);
    t.rounds[0].phase = SettlementPhase::Unresolved {
        last_definite: DefinitePhase::Finalized,
        problem: SettlementProblem::ReleaseCreditPending,
        application: Some(app.clone()),
        candidate: None,
    };
    let second = EncodedSnapshot::new(2, t.clone(), &limits).unwrap();
    assert_eq!(
        store.put_exact(Some(1), &second),
        WriteResolution::Durable(2)
    );
    app.reserved_credit = CostTuple::cents(100);
    app.applied_debit = CostTuple::ZERO;
    t.rounds[0].phase = SettlementPhase::Settled {
        application: app,
        receipt: None,
    };
    t.terminal = TurnTerminal::Cancelled;
    let last = EncodedSnapshot::new(3, t, &limits).unwrap();
    assert_eq!(store.put_exact(Some(2), &last), WriteResolution::Durable(3));
}

#[test]
fn pending_rollback_credit_can_advance_actual_credit_without_redebit() {
    use ardur_session_journals::CostTuple;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("store");
    let limits = SettlementLimits::default();
    let mut t = turn();
    let mut r = round(t.budget_epoch);
    let mut app = match r.phase {
        SettlementPhase::Settled { application, .. } => application,
        _ => unreachable!(),
    };
    app.rollback = RollbackStatus::Applied(CostTuple::cents(2));
    r.decision = Some(SettlementDecision::Completion { final_answer: true });
    r.phase = SettlementPhase::Unresolved {
        last_definite: DefinitePhase::Finalized,
        problem: SettlementProblem::RollbackCreditPending,
        application: Some(app.clone()),
        candidate: None,
    };
    t.rounds.push(r);
    t.terminal = TurnTerminal::Unresolved(SettlementProblem::RollbackCreditPending);
    let first = EncodedSnapshot::new(1, t.clone(), &limits).unwrap();
    let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
    assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
    app.rollback = RollbackStatus::Applied(app.applied_debit);
    t.rounds[0].phase = SettlementPhase::Settled {
        application: app,
        receipt: None,
    };
    t.terminal = TurnTerminal::Failed(InfrastructureFailureClass::Storage);
    let last = EncodedSnapshot::new(2, t.clone(), &limits).unwrap();
    assert_eq!(store.put_exact(Some(1), &last), WriteResolution::Durable(2));
    assert_eq!(
        load_settlement_snapshot(&root, &identity(), &limits)
            .unwrap()
            .snapshots[0]
            .turn(),
        &t
    );
}

fn tuple(axes: [u64; 5]) -> ardur_session_journals::CostTuple {
    ardur_session_journals::CostTuple {
        tokens_in: axes[0],
        tokens_out: axes[1],
        cents: axes[2],
        wall_ms: axes[3],
        attention_score: axes[4],
    }
}

fn rollback_fixture(
    decision: SettlementDecision,
    cents_only: bool,
    credited: ardur_session_journals::CostTuple,
) -> (TurnObligation, DebitApplication) {
    use ardur_session_journals::CostTuple;
    let amount = |n| {
        if cents_only {
            CostTuple::cents(n)
        } else {
            tuple([n; 5])
        }
    };
    let mut t = turn();
    let mut r = round(t.budget_epoch);
    r.reserved = amount(8);
    if let ProviderEvidence::Observed { cost, .. } = &mut r.provider_evidence {
        *cost = amount(10);
    }
    if let ToolEffect::Completed { cost, .. } = &mut r.tools[0].effect {
        *cost = amount(2);
    }
    r.known_incurred = amount(12);
    let cancelled = decision == SettlementDecision::Cancelled;
    let app = DebitApplication {
        epoch: t.budget_epoch,
        requested_debit: if cancelled {
            CostTuple::ZERO
        } else {
            amount(12)
        },
        applied_debit: amount(9),
        reserved_credit: CostTuple::ZERO,
        additional_debit: amount(1),
        shortfall: if cancelled {
            CostTuple::ZERO
        } else {
            amount(3)
        },
        rollback: RollbackStatus::Applied(credited),
    };
    r.decision = Some(decision);
    r.phase = SettlementPhase::Unresolved {
        last_definite: DefinitePhase::Finalized,
        problem: SettlementProblem::RollbackCreditPending,
        application: Some(app.clone()),
        candidate: None,
    };
    t.rounds.push(r);
    t.terminal = TurnTerminal::Unresolved(SettlementProblem::RollbackCreditPending);
    (t, app)
}

fn close_rollback(t: &mut TurnObligation, app: DebitApplication, with_receipt: bool) {
    let receipt = with_receipt.then(|| ReceiptBinding {
        receipt_id: ardur_session_journals::ReceiptId::new(),
        jws_digest: Sha256Digest::of(b"receipt-layer binding"),
    });
    t.terminal = match t.rounds[0].decision.as_ref().unwrap() {
        SettlementDecision::Refusal(class) => TurnTerminal::Refused(*class),
        SettlementDecision::Cancelled => TurnTerminal::Cancelled,
        _ => TurnTerminal::Failed(InfrastructureFailureClass::Storage),
    };
    t.rounds[0].phase = SettlementPhase::Settled {
        application: app,
        receipt,
    };
}

fn rollback_decisions() -> Vec<(SettlementDecision, bool)> {
    vec![
        (
            SettlementDecision::Refusal(RefusalClass::OutputBlocked),
            false,
        ),
        (
            SettlementDecision::InfrastructureFailure(InfrastructureFailureClass::Storage),
            false,
        ),
        (SettlementDecision::Cancelled, false),
        (
            SettlementDecision::Completion {
                final_answer: false,
            },
            false,
        ),
        (
            SettlementDecision::Completion {
                final_answer: false,
            },
            true,
        ),
        (SettlementDecision::Completion { final_answer: true }, false),
        (SettlementDecision::Completion { final_answer: true }, true),
    ]
}

fn partial_rollback_cases() -> [(&'static str, bool, [u64; 5]); 7] {
    [
        ("original-cents", true, [0, 0, 2, 0, 0]),
        ("tokens-in-only-remaining", false, [2, 9, 9, 9, 9]),
        ("tokens-out-only-remaining", false, [9, 2, 9, 9, 9]),
        ("cents-only-remaining", false, [9, 9, 2, 9, 9]),
        ("wall-only-remaining", false, [9, 9, 9, 2, 9]),
        ("attention-only-remaining", false, [9, 9, 9, 9, 2]),
        ("mixed", false, [2, 3, 4, 5, 6]),
    ]
}

// Match the canonical field order and payload digest, bypassing only semantic
// validation. The valid-byte control below prevents corruption from a bad wire
// fixture (ordering/digest/schema) from masquerading as the closure guard.
fn unchecked_snapshot_bytes(revision: u64, payload: &TurnObligation) -> Vec<u8> {
    #[derive(serde::Serialize)]
    struct WireSnapshot<'a> {
        revision: u64,
        payload_digest: Sha256Digest,
        payload: &'a TurnObligation,
    }
    serde_json::to_vec(&WireSnapshot {
        revision,
        payload_digest: Sha256Digest::of(&serde_json::to_vec(payload).unwrap()),
        payload,
    })
    .unwrap()
}

#[test]
fn spec_b2_partial_rollback_cannot_encode_or_persist_as_settled() {
    let mut violations = Vec::new();
    for (decision, receipt) in rollback_decisions() {
        for (name, cents_only, credit) in partial_rollback_cases() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap().join("store");
            let limits = SettlementLimits::default();
            let (mut t, app) = rollback_fixture(decision.clone(), cents_only, tuple(credit));
            let first = EncodedSnapshot::new(1, t.clone(), &limits).unwrap();
            let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
            assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
            let before = file_inventory(&root);
            close_rollback(&mut t, app, receipt);
            match EncodedSnapshot::new(2, t, &limits) {
                Err(SettlementStoreError::Invalid(SettlementProblem::Corrupt)) => {
                    assert_eq!(store.health(), StoreHealth::Healthy);
                    assert_eq!(file_inventory(&root), before);
                    assert_eq!(
                        load_settlement_snapshot(&root, &identity(), &limits)
                            .unwrap()
                            .snapshots[0]
                            .bytes(),
                        first.bytes()
                    );
                }
                Ok(premature) => {
                    let outcome = store.put_exact(Some(1), &premature);
                    violations.push(format!("{decision:?} receipt={receipt} {name}: premature encoding accepted, transition={outcome:?}, disk_unchanged={}", file_inventory(&root) == before));
                }
                Err(error) => panic!("unexpected error: {error:?}"),
            }
        }
    }
    assert!(
        violations.is_empty(),
        "partial full-debit rollback cannot close:\n{}",
        violations.join("\n")
    );
}

#[test]
fn spec_b2_canonical_partial_rollback_settled_inventory_is_rejected() {
    let mut violations = Vec::new();
    for (decision, receipt) in rollback_decisions() {
        for (name, cents_only, credit) in partial_rollback_cases() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap().join("store");
            let limits = SettlementLimits::default();
            let (mut t, app) = rollback_fixture(decision.clone(), cents_only, tuple(credit));
            let first = EncodedSnapshot::new(1, t.clone(), &limits).unwrap();
            assert_eq!(unchecked_snapshot_bytes(1, &t), first.bytes());
            let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
            assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
            drop(store);
            assert_eq!(
                load_settlement_snapshot(&root, &identity(), &limits)
                    .unwrap()
                    .snapshots[0]
                    .bytes(),
                first.bytes()
            );
            close_rollback(&mut t, app, receipt);
            let raw = unchecked_snapshot_bytes(2, &t);
            let path = root.join(format!("{}.json", t.turn_id.0));
            std::fs::write(&path, &raw).unwrap();
            let before = file_inventory(&root);
            let inventory = load_settlement_snapshot(&root, &identity(), &limits);
            let reopened = FileSettlementStore::open(&root, identity(), limits);
            if !matches!(
                inventory,
                Err(SettlementStoreError::Invalid(SettlementProblem::Corrupt))
            ) || !matches!(
                reopened,
                Err(SettlementStoreError::Invalid(SettlementProblem::Corrupt))
            ) {
                violations.push(format!("{decision:?} receipt={receipt} {name}: inventory_accepted={}, writer_opened={}", inventory.is_ok(), reopened.is_ok()));
            }
            assert_eq!(
                file_inventory(&root),
                before,
                "no implicit repair of malformed evidence"
            );
            assert_eq!(std::fs::read(&path).unwrap(), raw);
        }
    }
    assert!(
        violations.is_empty(),
        "canonical partial rollback must fail decode:\n{}",
        violations.join("\n")
    );
}

#[test]
fn spec_b2_partial_progress_and_full_rollback_remain_durable() {
    for (decision, with_receipt) in rollback_decisions() {
        if with_receipt {
            continue;
        }
        for cents_only in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap().join("store");
            let limits = SettlementLimits::default();
            let amount = |n| {
                if cents_only {
                    ardur_session_journals::CostTuple::cents(n)
                } else {
                    tuple([n; 5])
                }
            };
            let (mut t, mut app) = rollback_fixture(decision.clone(), cents_only, amount(2));
            let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
            let first = EncodedSnapshot::new(1, t.clone(), &limits).unwrap();
            assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
            app.rollback = RollbackStatus::Applied(amount(5));
            t.rounds[0].phase = SettlementPhase::Unresolved {
                last_definite: DefinitePhase::Finalized,
                problem: SettlementProblem::RollbackCreditPending,
                application: Some(app.clone()),
                candidate: None,
            };
            let second = EncodedSnapshot::new(2, t.clone(), &limits).unwrap();
            assert_eq!(
                store.put_exact(Some(1), &second),
                WriteResolution::Durable(2)
            );
            assert_eq!(
                load_settlement_snapshot(&root, &identity(), &limits)
                    .unwrap()
                    .snapshots[0]
                    .turn(),
                &t
            );
            app.rollback = RollbackStatus::Applied(app.applied_debit);
            close_rollback(&mut t, app, false);
            let last = EncodedSnapshot::new(3, t.clone(), &limits).unwrap();
            assert_eq!(store.put_exact(Some(2), &last), WriteResolution::Durable(3));
            drop(store);
            assert_eq!(
                load_settlement_snapshot(&root, &identity(), &limits)
                    .unwrap()
                    .snapshots[0]
                    .turn(),
                &t
            );
            let mut reopened = FileSettlementStore::open(&root, identity(), limits).unwrap();
            assert_eq!(
                reopened.resolve_exact(t.turn_id, 3, last.digest()),
                WriteResolution::Durable(3)
            );
        }
    }
}

#[test]
fn spec_b2_zero_debit_full_release_and_paid_refusal_controls() {
    use ardur_session_journals::CostTuple;
    for rollback in [
        RollbackStatus::None,
        RollbackStatus::Applied(CostTuple::ZERO),
    ] {
        for decision in [
            SettlementDecision::Refusal(RefusalClass::OutputBlocked),
            SettlementDecision::InfrastructureFailure(InfrastructureFailureClass::Storage),
            SettlementDecision::Cancelled,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap().join("store");
            let limits = SettlementLimits::default();
            let (mut t, mut app) = rollback_fixture(decision, false, CostTuple::ZERO);
            app.requested_debit = CostTuple::ZERO;
            app.applied_debit = CostTuple::ZERO;
            app.additional_debit = CostTuple::ZERO;
            app.reserved_credit = t.rounds[0].reserved;
            app.shortfall = CostTuple::ZERO;
            app.rollback = rollback.clone();
            close_rollback(&mut t, app, false);
            let snapshot = EncodedSnapshot::new(1, t.clone(), &limits).unwrap();
            let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
            assert_eq!(
                store.put_exact(None, &snapshot),
                WriteResolution::Durable(1)
            );
            assert_eq!(
                load_settlement_snapshot(&root, &identity(), &limits)
                    .unwrap()
                    .snapshots[0]
                    .turn(),
                &t
            );
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("store");
    let limits = SettlementLimits::default();
    let mut t = turn();
    t.rounds.push(round(t.budget_epoch));
    t.terminal = TurnTerminal::Refused(RefusalClass::OutputBlocked);
    let snapshot = EncodedSnapshot::new(1, t.clone(), &limits).unwrap();
    let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
    assert_eq!(
        store.put_exact(None, &snapshot),
        WriteResolution::Durable(1)
    );
    assert_eq!(
        load_settlement_snapshot(&root, &identity(), &limits)
            .unwrap()
            .snapshots[0]
            .turn(),
        &t
    );
}

fn release_decisions() -> Vec<SettlementDecision> {
    vec![
        SettlementDecision::Refusal(RefusalClass::OutputBlocked),
        SettlementDecision::InfrastructureFailure(InfrastructureFailureClass::Storage),
        SettlementDecision::Completion {
            final_answer: false,
        },
        SettlementDecision::Completion { final_answer: true },
        SettlementDecision::Cancelled,
    ]
}

fn partial_release_cases() -> Vec<(
    &'static str,
    ardur_session_journals::CostTuple,
    ardur_session_journals::CostTuple,
)> {
    vec![
        (
            "original-cents",
            tuple([0, 0, 100, 0, 0]),
            tuple([0, 0, 20, 0, 0]),
        ),
        (
            "tokens-in",
            tuple([100; 5]),
            tuple([20, 100, 100, 100, 100]),
        ),
        (
            "tokens-out",
            tuple([100; 5]),
            tuple([100, 20, 100, 100, 100]),
        ),
        ("cents", tuple([100; 5]), tuple([100, 100, 20, 100, 100])),
        ("wall", tuple([100; 5]), tuple([100, 100, 100, 20, 100])),
        (
            "attention",
            tuple([100; 5]),
            tuple([100, 100, 100, 100, 20]),
        ),
        ("mixed", tuple([100; 5]), tuple([20, 30, 40, 50, 60])),
    ]
}

fn release_fixture(
    decision: SettlementDecision,
    reserved: ardur_session_journals::CostTuple,
    credit: ardur_session_journals::CostTuple,
) -> (TurnObligation, DebitApplication) {
    use ardur_session_journals::CostTuple;
    let mut t = observed_turn();
    t.rounds[0].reserved = reserved;
    t.rounds[0].decision = Some(decision);
    let app = DebitApplication {
        epoch: t.budget_epoch,
        requested_debit: CostTuple::ZERO,
        applied_debit: reserved.checked_sub(&credit).unwrap(),
        reserved_credit: credit,
        additional_debit: CostTuple::ZERO,
        shortfall: CostTuple::ZERO,
        rollback: RollbackStatus::None,
    };
    retain_release(&mut t, app.clone());
    (t, app)
}

fn retain_release(t: &mut TurnObligation, application: DebitApplication) {
    t.rounds[0].phase = SettlementPhase::Unresolved {
        last_definite: DefinitePhase::Finalized,
        problem: SettlementProblem::ReleaseCreditPending,
        application: Some(application),
        candidate: None,
    };
    t.terminal = TurnTerminal::Unresolved(SettlementProblem::ReleaseCreditPending);
}

fn close_release(t: &mut TurnObligation, app: DebitApplication) {
    let completion = matches!(
        t.rounds[0].decision,
        Some(SettlementDecision::Completion { .. })
    );
    let receipt = completion.then(|| ReceiptBinding {
        receipt_id: ardur_session_journals::ReceiptId::new(),
        jws_digest: Sha256Digest::of(b"bound receipt"),
    });
    t.terminal = match t.rounds[0].decision.as_ref().unwrap() {
        SettlementDecision::Completion { final_answer: true } => {
            TurnTerminal::FinalAnswer(receipt.as_ref().unwrap().receipt_id)
        }
        SettlementDecision::Completion {
            final_answer: false,
        } => TurnTerminal::Open,
        SettlementDecision::Refusal(class) => TurnTerminal::Refused(*class),
        SettlementDecision::Cancelled => TurnTerminal::Cancelled,
        SettlementDecision::InfrastructureFailure(class) => TurnTerminal::Failed(*class),
    };
    t.rounds[0].phase = SettlementPhase::Settled {
        application: app,
        receipt,
    };
}

#[test]
fn quality_q3_partial_release_cannot_encode_or_persist_as_settled() {
    let mut violations = Vec::new();
    for decision in release_decisions() {
        for (name, reserved, credit) in partial_release_cases() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap().join("store");
            let limits = SettlementLimits::default();
            let (mut t, app) = release_fixture(decision.clone(), reserved, credit);
            let first = EncodedSnapshot::new(1, t.clone(), &limits).unwrap();
            let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
            assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
            let before = file_inventory(&root);
            close_release(&mut t, app);
            match EncodedSnapshot::new(2, t, &limits) {
                Err(SettlementStoreError::Invalid(SettlementProblem::Corrupt)) => {
                    assert_eq!(store.health(), StoreHealth::Healthy);
                    assert_eq!(file_inventory(&root), before);
                }
                Ok(next) => {
                    let result = store.put_exact(Some(1), &next);
                    violations.push(format!(
                        "{decision:?}/{name}: encoding accepted, put={result:?}, unchanged={}",
                        file_inventory(&root) == before
                    ));
                }
                Err(error) => panic!("unexpected {error:?}"),
            }
        }
    }
    assert!(
        violations.is_empty(),
        "unpaid reserve release cannot close:\n{}",
        violations.join("\n")
    );
}

#[test]
fn quality_q3_canonical_partial_release_inventory_and_reopen_are_rejected() {
    let mut violations = Vec::new();
    for decision in release_decisions() {
        for (name, reserved, credit) in partial_release_cases() {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap().join("store");
            let limits = SettlementLimits::default();
            let (mut t, app) = release_fixture(decision.clone(), reserved, credit);
            let first = EncodedSnapshot::new(1, t.clone(), &limits).unwrap();
            assert_eq!(
                unchecked_snapshot_bytes(1, &t),
                first.bytes(),
                "canonical wire control"
            );
            let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
            assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
            drop(store);
            assert_eq!(
                load_settlement_snapshot(&root, &identity(), &limits)
                    .unwrap()
                    .snapshots[0]
                    .bytes(),
                first.bytes()
            );
            close_release(&mut t, app);
            // Revision 1 tests a first retained snapshot, not just a transition.
            let raw = unchecked_snapshot_bytes(1, &t);
            std::fs::write(root.join(format!("{}.json", t.turn_id.0)), raw).unwrap();
            let before = file_inventory(&root);
            let inventory = load_settlement_snapshot(&root, &identity(), &limits);
            let reopened = FileSettlementStore::open(&root, identity(), limits);
            if !matches!(
                inventory,
                Err(SettlementStoreError::Invalid(SettlementProblem::Corrupt))
            ) || !matches!(
                reopened,
                Err(SettlementStoreError::Invalid(SettlementProblem::Corrupt))
            ) {
                violations.push(format!(
                    "{decision:?}/{name}: inventory={}, reopened={}",
                    inventory.is_ok(),
                    reopened.is_ok()
                ));
            }
            assert_eq!(file_inventory(&root), before, "no implicit repair");
        }
    }
    assert!(
        violations.is_empty(),
        "canonical release closure must fail decode:\n{}",
        violations.join("\n")
    );
}

#[test]
fn quality_q3_partial_progress_full_release_and_full_rollback_controls() {
    use ardur_session_journals::CostTuple;
    for decision in release_decisions() {
        for rollback in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().canonicalize().unwrap().join("store");
            let limits = SettlementLimits::default();
            let (mut t, mut app) =
                release_fixture(decision.clone(), tuple([100; 5]), tuple([20; 5]));
            let first = EncodedSnapshot::new(1, t.clone(), &limits).unwrap();
            let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
            assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
            app.reserved_credit = tuple([70; 5]);
            app.applied_debit = tuple([30; 5]);
            retain_release(&mut t, app.clone());
            let second = EncodedSnapshot::new(2, t.clone(), &limits).unwrap();
            assert_eq!(
                store.put_exact(Some(1), &second),
                WriteResolution::Durable(2)
            );
            assert_eq!(
                load_settlement_snapshot(&root, &identity(), &limits)
                    .unwrap()
                    .snapshots[0]
                    .turn(),
                &t
            );
            if rollback {
                app.rollback = RollbackStatus::Applied(app.applied_debit);
            } else {
                app.reserved_credit = tuple([100; 5]);
                app.applied_debit = CostTuple::ZERO;
            }
            close_release(&mut t, app);
            let last = EncodedSnapshot::new(3, t.clone(), &limits).unwrap();
            assert_eq!(store.put_exact(Some(2), &last), WriteResolution::Durable(3));
            drop(store);
            assert_eq!(
                load_settlement_snapshot(&root, &identity(), &limits)
                    .unwrap()
                    .snapshots[0]
                    .turn(),
                &t
            );
            let mut reopened = FileSettlementStore::open(&root, identity(), limits).unwrap();
            assert_eq!(
                reopened.resolve_exact(t.turn_id, 3, last.digest()),
                WriteResolution::Durable(3)
            );
        }
    }
}

#[test]
fn quality_q3_mixed_shortfall_is_not_an_unpaid_release_offset() {
    for decision in release_decisions()
        .into_iter()
        .filter(|d| *d != SettlementDecision::Cancelled)
    {
        let (mut t, mut app) = release_fixture(
            decision,
            tuple([8, 100, 100, 8, 100]),
            tuple([0, 20, 100, 0, 100]),
        );
        if let ProviderEvidence::Observed { cost, .. } = &mut t.rounds[0].provider_evidence {
            *cost = tuple([10; 5]);
        }
        if let ToolEffect::Completed { cost, .. } = &mut t.rounds[0].tools[0].effect {
            *cost = tuple([2; 5]);
        }
        t.rounds[0].known_incurred = tuple([12; 5]);
        app.requested_debit = tuple([12, 10, 0, 12, 0]);
        app.additional_debit = tuple([1, 0, 0, 1, 0]);
        app.applied_debit = tuple([9, 80, 0, 9, 0]);
        app.shortfall = tuple([3, 0, 0, 3, 0]);
        retain_release(&mut t, app.clone());
        let limits = SettlementLimits::default();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("store");
        let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
        let first = EncodedSnapshot::new(1, t.clone(), &limits).unwrap();
        assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
        let mut premature = t.clone();
        close_release(&mut premature, app.clone());
        assert!(matches!(
            EncodedSnapshot::new(2, premature, &limits),
            Err(SettlementStoreError::Invalid(SettlementProblem::Corrupt))
        ));
        // Fully return only excess reserve; preserve legitimate paid debit and
        // under-collection on the other axes without manufacturing extra credit.
        app.reserved_credit = tuple([0, 90, 100, 0, 100]);
        app.applied_debit = tuple([9, 10, 0, 9, 0]);
        close_release(&mut t, app);
        let closed = EncodedSnapshot::new(2, t.clone(), &limits).unwrap();
        assert_eq!(
            store.put_exact(Some(1), &closed),
            WriteResolution::Durable(2)
        );
        assert_eq!(
            load_settlement_snapshot(&root, &identity(), &limits)
                .unwrap()
                .snapshots[0]
                .turn(),
            &t
        );
    }
}

struct LeaseProbe {
    status: std::process::ExitStatus,
    timed_out: bool,
    reaped: bool,
    truncated: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl LeaseProbe {
    fn assert_success(&self, locked: bool) {
        let stdout = String::from_utf8_lossy(&self.stdout);
        assert!(
            !self.timed_out
                && self.reaped
                && !self.truncated
                && self.status.success()
                && stdout.contains("LEASE_ENTERED")
                && stdout.contains("LEASE_CLEANED")
                && stdout.contains(if locked {
                    "LEASE_BUSY"
                } else {
                    "LEASE_ACQUIRED"
                }),
            "lease child failed: timeout={}, reaped={}, status={}, stdout={}, stderr={}",
            self.timed_out,
            self.reaped,
            self.status,
            stdout.chars().take(512).collect::<String>(),
            String::from_utf8_lossy(&self.stderr[..self.stderr.len().min(512)])
        );
    }
}

// Drain continuously even after the retained diagnostics cap is reached.
fn drain_probe_pipe(mut reader: impl std::io::Read) -> (Vec<u8>, bool) {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 4096];
    loop {
        let count = reader.read(&mut chunk).unwrap();
        if count == 0 {
            break;
        }
        let keep = count.min(65_536usize.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&chunk[..keep]);
        truncated |= keep < count;
    }
    (bytes, truncated)
}

struct ReapProbe(std::process::Child);
impl Drop for ReapProbe {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn lease_probe(
    root: &std::path::Path,
    locked: bool,
    stall: bool,
    deadline: std::time::Duration,
) -> LeaseProbe {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    // Start from an allowlist: no provider credentials or inherited child modes.
    command.env_clear();
    for name in ["HOME", "PATH", "TMPDIR", "LANG"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .args([
            "--exact",
            "lifetime_writer_excludes_independent_handles_and_processes",
            "--nocapture",
        ])
        .env("SETTLEMENT_CHILD_ROOT", root);
    if locked {
        command.env("SETTLEMENT_CHILD_LOCKED", "1");
    }
    if stall {
        command.env("SETTLEMENT_CHILD_STALL", "1");
    }
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = ReapProbe(command.spawn().unwrap());
    let stdout = child.0.stdout.take().unwrap();
    let stderr = child.0.stderr.take().unwrap();
    let stdout = std::thread::spawn(move || drain_probe_pipe(stdout));
    let stderr = std::thread::spawn(move || drain_probe_pipe(stderr));
    let started = std::time::Instant::now();
    let (status, timed_out) = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break (status, false);
        }
        if started.elapsed() >= deadline {
            child.0.kill().unwrap();
            break (child.0.wait().unwrap(), true);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    let reaped = child.0.try_wait().unwrap().is_some();
    let (stdout, out_truncated) = stdout.join().unwrap();
    let (stderr, err_truncated) = stderr.join().unwrap();
    LeaseProbe {
        status,
        timed_out,
        reaped,
        truncated: out_truncated || err_truncated,
        stdout,
        stderr,
    }
}

#[test]
fn quality_lease_probe_deadline_kills_reaps_and_rejects_stalled_child() {
    use std::time::{Duration, Instant};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("store");
    drop(FileSettlementStore::open(&root, identity(), SettlementLimits::default()).unwrap());
    lease_probe(&root, false, false, Duration::from_secs(10)).assert_success(false);
    let started = Instant::now();
    let stalled = lease_probe(&root, false, true, Duration::from_secs(2));
    assert!(
        stalled.timed_out && stalled.reaped && started.elapsed() < Duration::from_secs(5),
        "stalled lease child must hit parent deadline, be killed and reaped"
    );
    assert!(String::from_utf8_lossy(&stalled.stdout).contains("LEASE_ENTERED"));
    assert!(stalled.truncated && stalled.stdout.len() <= 65_536 && stalled.stderr.len() <= 65_536);
    assert!(!String::from_utf8_lossy(&stalled.stdout).contains("LEASE_CLEANED"));
    assert!(
        std::panic::catch_unwind(|| stalled.assert_success(false)).is_err(),
        "a timed-out probe must fail the normal lease oracle"
    );
    lease_probe(&root, false, false, Duration::from_secs(10)).assert_success(false);
}

#[test]
fn lifetime_writer_excludes_independent_handles_and_processes() {
    if let Some(root) = std::env::var_os("SETTLEMENT_CHILD_ROOT") {
        println!("LEASE_ENTERED");
        if std::env::var_os("SETTLEMENT_CHILD_STALL").is_some() {
            // Exercise concurrent bounded pipe draining before a finite stall;
            // even the deliberately unfixed deadline harness remains bounded.
            use std::io::Write;
            std::io::stdout().write_all(&vec![b'x'; 262_144]).unwrap();
            std::io::stderr().write_all(&vec![b'y'; 262_144]).unwrap();
            std::thread::sleep(std::time::Duration::from_secs(10));
        }
        let expected_locked = std::env::var_os("SETTLEMENT_CHILD_LOCKED").is_some();
        let result = FileSettlementStore::open(
            std::path::Path::new(&root),
            identity(),
            SettlementLimits::default(),
        );
        if expected_locked {
            assert!(
                matches!(result, Err(SettlementStoreError::WriterBusy)),
                "child must specifically encounter a held writer lease"
            );
            println!("LEASE_BUSY");
        } else {
            assert!(
                result.is_ok(),
                "child must acquire after parent drops lease"
            );
            println!("LEASE_ACQUIRED");
        }
        drop(result);
        println!("LEASE_CLEANED");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("settlements");
    let mut store =
        FileSettlementStore::open(&root, identity(), SettlementLimits::default()).unwrap();
    let first = EncodedSnapshot::new(1, turn(), &SettlementLimits::default()).unwrap();
    assert_eq!(store.put_exact(None, &first), WriteResolution::Durable(1));
    let second =
        EncodedSnapshot::new(2, first.turn().clone(), &SettlementLimits::default()).unwrap();
    assert_eq!(
        store.put_exact(Some(1), &second),
        WriteResolution::Durable(2)
    );
    assert!(
        matches!(
            FileSettlementStore::open(&root, identity(), SettlementLimits::default()),
            Err(SettlementStoreError::WriterBusy)
        ),
        "independent writer must be rejected"
    );
    assert!(
        load_settlement_snapshot(&root, &identity(), &SettlementLimits::default()).is_ok(),
        "reader must not take writer lease"
    );
    let child = |locked: bool| {
        lease_probe(&root, locked, false, std::time::Duration::from_secs(10))
            .assert_success(locked);
    };
    child(true);
    drop(store);
    child(false);
}

#[test]
fn incompatible_receipt_identity_is_not_reinitialized() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("settlements");
    drop(FileSettlementStore::open(&root, identity(), SettlementLimits::default()).unwrap());
    let mut other = identity();
    other.signer = Sha256Digest::of(b"other signer");
    assert!(
        FileSettlementStore::open(&root, other, SettlementLimits::default()).is_err(),
        "signer rebinding must fail"
    );
    let mut other = identity();
    other.receipt_log = "/other/chain.jsonl".into();
    assert!(
        FileSettlementStore::open(&root, other, SettlementLimits::default()).is_err(),
        "receipt root rebinding must fail"
    );
}

#[test]
fn durable_snapshot_reopens_as_exact_cumulative_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("settlements");
    let limits = SettlementLimits::default();
    let snapshot = EncodedSnapshot::new(1, turn(), &limits).unwrap();
    let mut store = FileSettlementStore::open(&root, identity(), limits).unwrap();
    assert_eq!(
        store.put_exact(None, &snapshot),
        WriteResolution::Durable(1)
    );
    drop(store);
    let inventory = load_settlement_snapshot(&root, &identity(), &limits).unwrap();
    assert_eq!(inventory.snapshots.len(), 1);
    assert_eq!(inventory.snapshots[0].bytes(), snapshot.bytes());
    assert_eq!(inventory.snapshots[0].digest(), snapshot.digest());
    let mut reopened = FileSettlementStore::open(&root, identity(), limits).unwrap();
    assert_eq!(
        reopened.resolve_exact(snapshot.turn().turn_id, 1, snapshot.digest()),
        WriteResolution::Durable(1)
    );
    assert_eq!(
        reopened.put_exact(None, &snapshot),
        WriteResolution::Durable(1)
    );
}
