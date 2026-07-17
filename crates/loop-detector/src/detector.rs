//! The three-signal detector and its per-session state.
//!
//! The detector is pure: [`LoopDetector`] methods take `&self` (the detector
//! holds only its whitelist evaluator) and a `&mut LoopDetectorState` carrying
//! all the sliding-window history. That keeps detection deterministic and lets
//! the owning runtime own persistence — it serializes [`LoopDetectorState`] to
//! §7.0 memory for crash-resilient detection across a session resume.

use std::collections::{BTreeSet, VecDeque};

use ardur_receipt::{CostTuple, Sha256Digest};
use serde::{Deserialize, Serialize};

use crate::budget::LoopBudget;
use crate::error::DetectorError;
use crate::ids::{RunId, SessionId, TurnId};
use crate::sealed::Sealed;
use crate::types::{
    DetectorVerdict, HaltStatus, KillReason, LoopEvidence, LoopSignature, OverrideAction, Signal,
    SignalKind, WhitelistEntry,
};
use crate::whitelist::{DefaultWhitelistEvaluator, WhitelistEvaluator};

/// One tool-call admission the detector observes. The caller computes the args
/// fingerprint with [`crate::args_fingerprint`] and supplies the admission
/// receipt's hash so the detector can name offending receipts in its evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolAdmission {
    /// The turn this admission belongs to.
    pub turn: TurnId,
    /// The tool being admitted.
    pub tool_name: String,
    /// Canonical fingerprint of the tool arguments.
    pub args_fingerprint: Sha256Digest,
    /// Hash of the `tool.call.admitted.v1` receipt for this admission.
    pub receipt_hash: Sha256Digest,
    /// Whether the admission carries a polling key (for polling whitelist).
    pub has_polling_key: bool,
    /// Whether the admission carries a pagination cursor (for pagination whitelist).
    pub has_pagination_cursor: bool,
}

/// The close-of-turn record the detector observes to evaluate the no-progress
/// and cost-acceleration signals.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnRecord {
    /// The turn being closed.
    pub turn: TurnId,
    /// The turn's total cost (the §11.14 cost tuple the runtime already computed).
    pub cost: CostTuple,
    /// Whether the turn produced a progress receipt (memory write, channel
    /// outbound, checkpoint, or child completion).
    pub made_progress: bool,
}

/// Per-session detector state. All windows are turn-bounded; the state is
/// serializable so the runtime can persist and resume it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoopDetectorState {
    /// The session this state tracks.
    pub session_id: SessionId,
    /// The run this state tracks.
    pub run_id: RunId,
    /// The active thresholds (from the run's cap-token loop budget).
    pub effective_budget: LoopBudget,
    /// Same-tool-same-args sliding window: `(signature, admission receipt, turn)`.
    same_tool_window: VecDeque<(LoopSignature, Sha256Digest, TurnId)>,
    /// Recent per-turn cost, for the cost-acceleration signal and evidence.
    cost_window: VecDeque<(TurnId, CostTuple)>,
    /// Consecutive no-progress turns observed.
    consecutive_no_progress: u32,
    /// Consecutive accelerating cost windows observed.
    consecutive_cost_accel: u32,
    /// The signal kinds currently tripped (a set — no double-counting).
    active_trips: BTreeSet<SignalKind>,
    /// The most recent turn observed, enforcing a monotonic clock.
    last_turn: Option<TurnId>,
    /// The run's halt status.
    pub halt_status: HaltStatus,
    /// Per-session repetition whitelist (from the runtime profile).
    pub whitelist: Vec<WhitelistEntry>,
}

impl LoopDetectorState {
    /// A fresh healthy state for a run under `budget`.
    pub fn new(session_id: SessionId, run_id: RunId, budget: LoopBudget) -> Self {
        Self {
            session_id,
            run_id,
            effective_budget: budget,
            same_tool_window: VecDeque::new(),
            cost_window: VecDeque::new(),
            consecutive_no_progress: 0,
            consecutive_cost_accel: 0,
            active_trips: BTreeSet::new(),
            last_turn: None,
            halt_status: HaltStatus::Healthy,
            whitelist: Vec::new(),
        }
    }

    fn check_monotonic(&self, turn: TurnId) -> Result<(), DetectorError> {
        if let Some(last) = self.last_turn
            && turn < last
        {
            return Err(DetectorError::NonMonotonicTurn {
                observed: turn.0,
                last: last.0,
            });
        }
        Ok(())
    }

    fn cost_trajectory(&self) -> Vec<(TurnId, CostTuple)> {
        self.cost_window.iter().cloned().collect()
    }

    /// Clear every active signal trip. Used by the override flow when an
    /// operator resumes a halted run; the windows are left intact, so a loop
    /// that persists trips again.
    pub fn clear_active_trips(&mut self) {
        self.active_trips.clear();
    }

    /// The signal kinds currently tripped, for operator inspection.
    pub fn active_trips(&self) -> impl Iterator<Item = SignalKind> + '_ {
        self.active_trips.iter().copied()
    }
}

/// The detector facade. Sealed: [`InMemoryLoopDetector`] is the single workspace
/// implementation, so the safety logic is single-sourced and auditable.
pub trait LoopDetector: Sealed {
    /// Observe a tool-call admission and evaluate the same-tool-same-args signal
    /// (and detect a continued-after-halt kill).
    fn observe_admission(
        &self,
        admission: &ToolAdmission,
        state: &mut LoopDetectorState,
    ) -> Result<DetectorVerdict, DetectorError>;

    /// Observe a closed turn and evaluate the no-progress and cost-acceleration
    /// signals, clearing a no-progress trip when the turn made progress.
    fn observe_turn(
        &self,
        record: &TurnRecord,
        state: &mut LoopDetectorState,
    ) -> Result<DetectorVerdict, DetectorError>;

    /// Evaluate whether an open grace window has expired without the signal
    /// clearing, escalating a still-active trip to a halt.
    fn check_grace_expiry(
        &self,
        current_turn: TurnId,
        state: &mut LoopDetectorState,
    ) -> Result<DetectorVerdict, DetectorError>;
}

/// The Phase-1 in-memory detector. Holds only its whitelist evaluator; all
/// history lives in the caller-owned [`LoopDetectorState`].
#[derive(Clone, Copy, Debug, Default)]
pub struct InMemoryLoopDetector {
    whitelist: DefaultWhitelistEvaluator,
}

impl InMemoryLoopDetector {
    /// Construct the default detector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a trip and decide the escalation verdict per the active override
    /// action and the number of jointly-active signals.
    fn on_trip(
        &self,
        signal: Signal,
        offending: Vec<Sha256Digest>,
        current_turn: TurnId,
        state: &mut LoopDetectorState,
    ) -> DetectorVerdict {
        let evidence = LoopEvidence {
            session_id: state.session_id,
            run_id: state.run_id,
            signal: signal.clone(),
            offending_receipts: offending,
            cost_trajectory: state.cost_trajectory(),
        };
        state.active_trips.insert(signal.kind());

        // Warn-only profiles emit the detected verdict but never halt: leave the
        // status Healthy so no grace window escalates.
        if state.effective_budget.override_action == OverrideAction::Warn {
            return DetectorVerdict::SignalTripped { signal, evidence };
        }

        // Emergency cost brake: a signal that doubles the base ratio kills now.
        if let Signal::CostAcceleration { ratio, .. } = &signal
            && *ratio >= state.effective_budget.cost_acceleration_ratio * 2.0
        {
            state.halt_status = HaltStatus::Killed {
                reason: KillReason::EmergencyCostAcceleration,
            };
            return DetectorVerdict::KillRequired {
                reason: KillReason::EmergencyCostAcceleration,
                evidence,
            };
        }

        // Multiple signals active at once, or a Kill-mode profile, escalate
        // straight to a kill.
        if state.active_trips.len() >= 2 {
            let reason = KillReason::MultiSignal {
                signals: state.active_trips.iter().copied().collect(),
            };
            state.halt_status = HaltStatus::Killed {
                reason: reason.clone(),
            };
            return DetectorVerdict::KillRequired { reason, evidence };
        }
        if state.effective_budget.override_action == OverrideAction::Kill {
            let reason = KillReason::MultiSignal {
                signals: vec![signal.kind()],
            };
            state.halt_status = HaltStatus::Killed {
                reason: reason.clone(),
            };
            return DetectorVerdict::KillRequired { reason, evidence };
        }

        // Halt-mode single signal: open the grace window.
        state.halt_status = HaltStatus::Detected {
            signal: signal.clone(),
            since_turn: current_turn,
        };
        DetectorVerdict::SignalTripped { signal, evidence }
    }
}

impl Sealed for InMemoryLoopDetector {}

impl LoopDetector for InMemoryLoopDetector {
    fn observe_admission(
        &self,
        admission: &ToolAdmission,
        state: &mut LoopDetectorState,
    ) -> Result<DetectorVerdict, DetectorError> {
        state.check_monotonic(admission.turn)?;
        state.last_turn = Some(admission.turn);

        // A run that keeps requesting admissions after it was halted is ignoring
        // the halt — escalate to a kill.
        if matches!(state.halt_status, HaltStatus::Halted { .. }) {
            let evidence = LoopEvidence {
                session_id: state.session_id,
                run_id: state.run_id,
                signal: match &state.halt_status {
                    HaltStatus::Halted { signal } => signal.clone(),
                    _ => unreachable!("guarded by matches! above"),
                },
                offending_receipts: vec![admission.receipt_hash],
                cost_trajectory: state.cost_trajectory(),
            };
            state.halt_status = HaltStatus::Killed {
                reason: KillReason::ContinuedAfterHalt,
            };
            return Ok(DetectorVerdict::KillRequired {
                reason: KillReason::ContinuedAfterHalt,
                evidence,
            });
        }
        if matches!(state.halt_status, HaltStatus::Killed { .. }) {
            return Ok(DetectorVerdict::Continue);
        }

        // Whitelisted repetition (polling/pagination/blanket) is not tracked for
        // Signal 1, but the tool still counts toward Signals 2 and 3.
        if self.whitelist.is_exempt(
            &admission.tool_name,
            admission.has_polling_key,
            admission.has_pagination_cursor,
            &state.whitelist,
        ) {
            return Ok(DetectorVerdict::Continue);
        }

        let signature = LoopSignature {
            tool_name: admission.tool_name.clone(),
            args_fingerprint: admission.args_fingerprint,
        };
        let window = state.effective_budget.same_tool_same_args_window_turns as u64;
        // Evict entries older than the window before recording the new one.
        while let Some((_, _, t)) = state.same_tool_window.front() {
            if admission.turn.since(*t) >= window {
                state.same_tool_window.pop_front();
            } else {
                break;
            }
        }
        state.same_tool_window.push_back((
            signature.clone(),
            admission.receipt_hash,
            admission.turn,
        ));

        let offending: Vec<Sha256Digest> = state
            .same_tool_window
            .iter()
            .filter(|(sig, _, _)| *sig == signature)
            .map(|(_, hash, _)| *hash)
            .collect();
        let count = offending.len() as u32;
        if count >= state.effective_budget.same_tool_same_args_count_threshold {
            let signal = Signal::SameToolSameArgs { signature, count };
            return Ok(self.on_trip(signal, offending, admission.turn, state));
        }
        Ok(DetectorVerdict::Continue)
    }

    fn observe_turn(
        &self,
        record: &TurnRecord,
        state: &mut LoopDetectorState,
    ) -> Result<DetectorVerdict, DetectorError> {
        state.check_monotonic(record.turn)?;
        state.last_turn = Some(record.turn);
        if matches!(state.halt_status, HaltStatus::Killed { .. }) {
            return Ok(DetectorVerdict::Continue);
        }

        // --- Signal 2: no progress ---
        if record.made_progress {
            state.consecutive_no_progress = 0;
            // Progress clears a no-progress trip; if that empties the active set
            // and we were only in a detected state, return to healthy.
            if state.active_trips.remove(&SignalKind::NoProgress)
                && state.active_trips.is_empty()
                && matches!(state.halt_status, HaltStatus::Detected { .. })
            {
                state.halt_status = HaltStatus::Healthy;
            }
        } else {
            state.consecutive_no_progress = state.consecutive_no_progress.saturating_add(1);
        }

        // --- Signal 3: cost acceleration (record cost first) ---
        state.cost_window.push_back((record.turn, record.cost));
        // Keep the window bounded; the check only needs a small lookback.
        while state.cost_window.len() > 16 {
            state.cost_window.pop_front();
        }
        let mut cost_signal: Option<(Signal, f32)> = None;
        let len = state.cost_window.len();
        if len >= 4 {
            let cur = turn_cost(&state.cost_window[len - 1].1);
            let prev = turn_cost(&state.cost_window[len - 4].1);
            let ratio = growth_ratio(prev, cur);
            if ratio > state.effective_budget.cost_acceleration_ratio {
                state.consecutive_cost_accel = state.consecutive_cost_accel.saturating_add(1);
            } else {
                state.consecutive_cost_accel = 0;
            }
            if state.consecutive_cost_accel >= state.effective_budget.cost_acceleration_windows {
                cost_signal = Some((
                    Signal::CostAcceleration {
                        ratio,
                        consecutive_windows: state.consecutive_cost_accel,
                    },
                    ratio,
                ));
            }
        }

        // Prefer the cost-acceleration trip (it can escalate to an emergency
        // kill); otherwise evaluate the no-progress threshold.
        if let Some((signal, _)) = cost_signal {
            return Ok(self.on_trip(signal, Vec::new(), record.turn, state));
        }
        if state.consecutive_no_progress >= state.effective_budget.no_progress_turns_threshold {
            let signal = Signal::NoProgress {
                consecutive_turns: state.consecutive_no_progress,
            };
            return Ok(self.on_trip(signal, Vec::new(), record.turn, state));
        }
        Ok(DetectorVerdict::Continue)
    }

    fn check_grace_expiry(
        &self,
        current_turn: TurnId,
        state: &mut LoopDetectorState,
    ) -> Result<DetectorVerdict, DetectorError> {
        let (signal, since_turn) = match &state.halt_status {
            HaltStatus::Detected { signal, since_turn } => (signal.clone(), *since_turn),
            _ => return Err(DetectorError::NoGraceWindow),
        };
        // Signal recovered within grace (e.g. progress cleared a no-progress
        // trip): nothing to halt.
        if !state.active_trips.contains(&signal.kind()) {
            state.halt_status = HaltStatus::Healthy;
            return Ok(DetectorVerdict::Continue);
        }
        if current_turn.since(since_turn) > state.effective_budget.grace_window_turns as u64 {
            let evidence = LoopEvidence {
                session_id: state.session_id,
                run_id: state.run_id,
                signal: signal.clone(),
                offending_receipts: Vec::new(),
                cost_trajectory: state.cost_trajectory(),
            };
            state.halt_status = HaltStatus::Halted {
                signal: signal.clone(),
            };
            return Ok(DetectorVerdict::HaltRequired { signal, evidence });
        }
        Ok(DetectorVerdict::Continue)
    }
}

/// The per-turn cost scalar the acceleration signal compares: total tokens.
fn turn_cost(cost: &CostTuple) -> u64 {
    cost.tokens_in.saturating_add(cost.tokens_out)
}

/// Growth ratio `cur / prev`, defined so a rise from zero reads as large and a
/// flat-zero window reads as no growth.
fn growth_ratio(prev: u64, cur: u64) -> f32 {
    if prev == 0 {
        if cur == 0 { 1.0 } else { f32::INFINITY }
    } else {
        cur as f32 / prev as f32
    }
}
