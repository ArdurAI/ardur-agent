//! §11.13 end-to-end coverage of the public surface: the three signals, the
//! grace→halt→kill escalation, cap-token budget attenuation, the operator
//! override, and the repetition whitelist.

use ardur_loop_detector::{
    DetectorVerdict, HaltStatus, InMemoryLoopDetector, InMemoryRunawayHalter, KillReason,
    LoopBudget, LoopBudgetRequest, LoopDetector, LoopDetectorState, OverrideAction,
    OverrideRequest, RunId, RunawayHalter, SessionId, Signal, ToolAdmission, TurnId, TurnRecord,
    WhitelistEntry, WhitelistKind, args_fingerprint, derive_for_loop_budget,
};
use ardur_receipt::{CostTuple, Sha256Digest};
use serde_json::json;
use uuid::Uuid;

fn fresh_state(budget: LoopBudget) -> LoopDetectorState {
    LoopDetectorState::new(SessionId(Uuid::nil()), RunId(Uuid::nil()), budget)
}

fn admission(turn: u64, tool: &str, args: &serde_json::Value) -> ToolAdmission {
    ToolAdmission {
        turn: TurnId(turn),
        tool_name: tool.to_string(),
        args_fingerprint: args_fingerprint(args, &[]),
        receipt_hash: Sha256Digest::of(format!("r{turn}").as_bytes()),
        has_polling_key: false,
        has_pagination_cursor: false,
    }
}

fn turn(turn: u64, tokens: u64, made_progress: bool) -> TurnRecord {
    TurnRecord {
        turn: TurnId(turn),
        cost: CostTuple {
            tokens_in: tokens,
            tokens_out: 0,
            cents: 0,
            wall_ms: 0,
            attention_score: 0.0,
        },
        made_progress,
    }
}

#[test]
fn same_tool_same_args_trips_at_threshold() {
    let det = InMemoryLoopDetector::new();
    let mut state = fresh_state(LoopBudget::default()); // N = 5
    let args = json!({"query": "rust loops"});

    for t in 1..=4 {
        assert_eq!(
            det.observe_admission(&admission(t, "web_search", &args), &mut state)
                .unwrap(),
            DetectorVerdict::Continue,
            "turn {t} should not yet trip"
        );
    }
    // The 5th identical admission crosses N = 5.
    match det
        .observe_admission(&admission(5, "web_search", &args), &mut state)
        .unwrap()
    {
        DetectorVerdict::SignalTripped {
            signal: Signal::SameToolSameArgs { count, .. },
            evidence,
        } => {
            assert_eq!(count, 5);
            assert_eq!(evidence.offending_receipts.len(), 5);
        }
        other => panic!("expected SameToolSameArgs trip, got {other:?}"),
    }
    assert!(matches!(state.halt_status, HaltStatus::Detected { .. }));
}

#[test]
fn varying_args_does_not_trip_repetition() {
    let det = InMemoryLoopDetector::new();
    let mut state = fresh_state(LoopBudget::default());
    for t in 1..=8 {
        let args = json!({"page": t});
        assert_eq!(
            det.observe_admission(&admission(t, "list", &args), &mut state)
                .unwrap(),
            DetectorVerdict::Continue
        );
    }
}

#[test]
fn no_progress_trips_after_k_turns() {
    let det = InMemoryLoopDetector::new();
    let mut state = fresh_state(LoopBudget::default()); // K = 8
    for t in 1..=7 {
        assert_eq!(
            det.observe_turn(&turn(t, 10, false), &mut state).unwrap(),
            DetectorVerdict::Continue,
            "turn {t}"
        );
    }
    match det.observe_turn(&turn(8, 10, false), &mut state).unwrap() {
        DetectorVerdict::SignalTripped {
            signal: Signal::NoProgress { consecutive_turns },
            ..
        } => assert_eq!(consecutive_turns, 8),
        other => panic!("expected NoProgress trip, got {other:?}"),
    }
}

#[test]
fn progress_receipt_resets_no_progress() {
    let det = InMemoryLoopDetector::new();
    let mut state = fresh_state(LoopBudget::default());
    for t in 1..=7 {
        det.observe_turn(&turn(t, 10, false), &mut state).unwrap();
    }
    // A progress receipt on turn 8 resets the counter; turn 9 stays healthy.
    det.observe_turn(&turn(8, 10, true), &mut state).unwrap();
    assert_eq!(
        det.observe_turn(&turn(9, 10, false), &mut state).unwrap(),
        DetectorVerdict::Continue
    );
}

#[test]
fn cost_acceleration_trips_after_w_windows() {
    let det = InMemoryLoopDetector::new();
    let mut state = fresh_state(LoopBudget::default()); // R = 2.0, W = 4
    // Geometric growth by 2.5 per 3-turn lookback keeps each window ratio in
    // (R, 2R): trips on the 4th sustained window without an emergency kill.
    // Progress = true isolates this from the no-progress signal.
    let costs = [10u64, 10, 10, 25, 25, 25, 63];
    let mut verdicts = Vec::new();
    for (i, c) in costs.iter().enumerate() {
        let t = (i + 1) as u64;
        verdicts.push(det.observe_turn(&turn(t, *c, true), &mut state).unwrap());
    }
    match verdicts.last().unwrap() {
        DetectorVerdict::SignalTripped {
            signal:
                Signal::CostAcceleration {
                    consecutive_windows,
                    ..
                },
            ..
        } => assert!(*consecutive_windows >= 4),
        other => panic!("expected CostAcceleration trip, got {other:?}"),
    }
}

#[test]
fn grace_window_expires_into_a_halt() {
    let det = InMemoryLoopDetector::new();
    let halter = InMemoryRunawayHalter::new();
    let mut state = fresh_state(LoopBudget::default()); // grace = 1
    let args = json!({"q": "x"});
    for t in 1..=5 {
        det.observe_admission(&admission(t, "web_search", &args), &mut state)
            .unwrap();
    }
    assert!(matches!(state.halt_status, HaltStatus::Detected { .. }));
    // Within grace (turn 6 = since+1) the run continues.
    assert_eq!(
        det.check_grace_expiry(TurnId(6), &mut state).unwrap(),
        DetectorVerdict::Continue
    );
    // Past grace (turn 7 > since+1) the halt fires.
    match det.check_grace_expiry(TurnId(7), &mut state).unwrap() {
        DetectorVerdict::HaltRequired { evidence, .. } => {
            let report = halter.halt(&evidence).unwrap();
            assert_eq!(report.verb.as_str(), "agent.loop.halted.v1");
        }
        other => panic!("expected HaltRequired, got {other:?}"),
    }
    assert!(matches!(state.halt_status, HaltStatus::Halted { .. }));
}

#[test]
fn admission_after_halt_escalates_to_kill() {
    let det = InMemoryLoopDetector::new();
    let mut state = fresh_state(LoopBudget::default());
    let args = json!({"q": "x"});
    for t in 1..=5 {
        det.observe_admission(&admission(t, "web_search", &args), &mut state)
            .unwrap();
    }
    det.check_grace_expiry(TurnId(7), &mut state).unwrap(); // → Halted
    match det
        .observe_admission(&admission(8, "web_search", &args), &mut state)
        .unwrap()
    {
        DetectorVerdict::KillRequired {
            reason: KillReason::ContinuedAfterHalt,
            ..
        } => {}
        other => panic!("expected ContinuedAfterHalt kill, got {other:?}"),
    }
    assert!(matches!(state.halt_status, HaltStatus::Killed { .. }));
}

#[test]
fn two_signals_escalate_straight_to_kill() {
    let det = InMemoryLoopDetector::new();
    // Tighten K so the no-progress signal can fire alongside repetition.
    let budget = LoopBudget {
        no_progress_turns_threshold: 3,
        ..LoopBudget::default()
    };
    let mut state = fresh_state(budget);
    let args = json!({"q": "x"});
    // Trip repetition first (turns 1..=5).
    for t in 1..=5 {
        det.observe_admission(&admission(t, "web_search", &args), &mut state)
            .unwrap();
    }
    assert!(matches!(state.halt_status, HaltStatus::Detected { .. }));
    // Now trip no-progress on the same run — two active signals → kill.
    for t in 6..=8 {
        let v = det.observe_turn(&turn(t, 10, false), &mut state).unwrap();
        if let DetectorVerdict::KillRequired {
            reason: KillReason::MultiSignal { .. },
            ..
        } = v
        {
            assert!(matches!(state.halt_status, HaltStatus::Killed { .. }));
            return;
        }
    }
    panic!("expected a MultiSignal kill");
}

#[test]
fn warn_mode_never_halts() {
    let det = InMemoryLoopDetector::new();
    let budget = LoopBudget {
        override_action: OverrideAction::Warn,
        ..LoopBudget::default()
    };
    let mut state = fresh_state(budget);
    let args = json!({"q": "x"});
    for t in 1..=6 {
        det.observe_admission(&admission(t, "web_search", &args), &mut state)
            .unwrap();
    }
    // A Warn profile emits the detected verdict but stays healthy.
    assert!(matches!(state.halt_status, HaltStatus::Healthy));
}

#[test]
fn operator_override_resumes_a_halted_run() {
    let det = InMemoryLoopDetector::new();
    let halter = InMemoryRunawayHalter::new();
    let mut state = fresh_state(LoopBudget::default());
    let args = json!({"q": "x"});
    for t in 1..=5 {
        det.observe_admission(&admission(t, "web_search", &args), &mut state)
            .unwrap();
    }
    det.check_grace_expiry(TurnId(7), &mut state).unwrap(); // → Halted

    // Empty reason is refused.
    let bad = OverrideRequest {
        run_id: state.run_id,
        reason: "  ".into(),
    };
    assert!(halter.override_halt(&bad, &mut state).is_err());

    let ok = OverrideRequest {
        run_id: state.run_id,
        reason: "reviewed: legitimate research paging".into(),
    };
    let report = halter.override_halt(&ok, &mut state).unwrap();
    assert_eq!(report.verb.as_str(), "agent.loop.detection_overridden.v1");
    assert!(matches!(state.halt_status, HaltStatus::Healthy));
    assert_eq!(state.active_trips().count(), 0);
}

#[test]
fn whitelisted_poller_is_exempt_from_repetition() {
    let det = InMemoryLoopDetector::new();
    let mut state = fresh_state(LoopBudget::default());
    state.whitelist.push(WhitelistEntry {
        tool_name: "job_status".into(),
        kind: WhitelistKind::Polling,
    });
    let args = json!({"job": "abc"});
    for t in 1..=10 {
        let mut adm = admission(t, "job_status", &args);
        adm.has_polling_key = true;
        assert_eq!(
            det.observe_admission(&adm, &mut state).unwrap(),
            DetectorVerdict::Continue,
            "polling turn {t} must stay exempt"
        );
    }
    assert!(matches!(state.halt_status, HaltStatus::Healthy));
}

#[test]
fn loop_budget_attenuation_is_monotonic() {
    let parent = LoopBudget::default(); // N = 5
    // Tightening is allowed.
    let tighter = derive_for_loop_budget(
        &parent,
        &LoopBudgetRequest {
            same_tool_same_args_count_threshold: Some(3),
            override_action: Some(OverrideAction::Kill),
            ..LoopBudgetRequest::default()
        },
    )
    .unwrap();
    assert_eq!(tighter.same_tool_same_args_count_threshold, 3);
    assert_eq!(tighter.override_action, OverrideAction::Kill);

    // Relaxing N is refused.
    assert!(
        derive_for_loop_budget(
            &parent,
            &LoopBudgetRequest {
                same_tool_same_args_count_threshold: Some(10),
                ..LoopBudgetRequest::default()
            },
        )
        .is_err()
    );
    // Relaxing the override action (Halt → Warn) is refused.
    assert!(
        derive_for_loop_budget(
            &parent,
            &LoopBudgetRequest {
                override_action: Some(OverrideAction::Warn),
                ..LoopBudgetRequest::default()
            },
        )
        .is_err()
    );
}

#[test]
fn detector_state_round_trips_through_serde() {
    let det = InMemoryLoopDetector::new();
    let mut state = fresh_state(LoopBudget::default());
    let args = json!({"q": "x"});
    for t in 1..=3 {
        det.observe_admission(&admission(t, "web_search", &args), &mut state)
            .unwrap();
    }
    let json = serde_json::to_string(&state).unwrap();
    let restored: LoopDetectorState = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, state);
}
