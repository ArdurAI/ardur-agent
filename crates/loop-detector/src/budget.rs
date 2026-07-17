//! The cap-token-encoded [`LoopBudget`] and its monotonic attenuation.
//!
//! ADR-Phase3-274: every cap-token issued for an agent run carries a
//! `LoopBudget`. A derived (child) budget may tighten any field but never relax
//! it, so a sub-agent inherits — and can only shrink — its parent's loop
//! tolerance. [`derive_for_loop_budget`] is the single enforcement point; §5.0's
//! child-mission derivation routes through it.

use crate::error::BudgetError;
use crate::types::OverrideAction;
use serde::{Deserialize, Serialize};

/// The thresholds the detector evaluates its three signals against, carried on
/// the run's cap-token. Defaults are conservative — chosen to catch obvious
/// loops without false-positiving normal iterative work.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoopBudget {
    /// `N`: same-tool-same-args occurrences within the window that trip Signal 1.
    pub same_tool_same_args_count_threshold: u32,
    /// `M`: the Signal-1 sliding window, in turns.
    pub same_tool_same_args_window_turns: u32,
    /// `K`: consecutive no-progress turns that trip Signal 2.
    pub no_progress_turns_threshold: u32,
    /// `R`: the per-turn cost growth factor that a window must exceed for Signal 3.
    pub cost_acceleration_ratio: f32,
    /// `W`: consecutive accelerating windows required to trip Signal 3.
    pub cost_acceleration_windows: u32,
    /// Turns the run may continue after a trip before a halt fires.
    pub grace_window_turns: u32,
    /// What a trip does: halt (default), warn-only (dev/canary), or kill outright.
    pub override_action: OverrideAction,
}

impl Default for LoopBudget {
    fn default() -> Self {
        Self {
            same_tool_same_args_count_threshold: 5,
            same_tool_same_args_window_turns: 10,
            no_progress_turns_threshold: 8,
            cost_acceleration_ratio: 2.0,
            cost_acceleration_windows: 4,
            grace_window_turns: 1,
            override_action: OverrideAction::Halt,
        }
    }
}

/// A requested child budget. Every field is optional; `None` inherits the
/// parent's value, `Some` must be a tightening of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LoopBudgetRequest {
    /// Requested `N` (must be `<=` parent).
    pub same_tool_same_args_count_threshold: Option<u32>,
    /// Requested `M` (must be `<=` parent).
    pub same_tool_same_args_window_turns: Option<u32>,
    /// Requested `K` (must be `<=` parent).
    pub no_progress_turns_threshold: Option<u32>,
    /// Requested `R` (must be `<=` parent — a lower ratio trips sooner).
    pub cost_acceleration_ratio: Option<f32>,
    /// Requested `W` (must be `<=` parent).
    pub cost_acceleration_windows: Option<u32>,
    /// Requested grace (must be `<=` parent — less grace is stricter).
    pub grace_window_turns: Option<u32>,
    /// Requested action (must be at least as aggressive as parent).
    pub override_action: Option<OverrideAction>,
}

/// Derive a child budget from a parent, enforcing monotonic narrowing.
///
/// Every numeric field may only be lowered (a lower threshold, window, ratio, or
/// grace all make the detector strictly stricter), and the override action may
/// only move toward more aggressive handling ([`OverrideAction::severity`]).
/// Any relaxation is a hard [`BudgetError::RelaxationAttempted`] — the caller
/// (e.g. §5.0's child-mission derivation) must not be able to hand a sub-agent a
/// looser loop budget than it holds itself.
pub fn derive_for_loop_budget(
    parent: &LoopBudget,
    req: &LoopBudgetRequest,
) -> Result<LoopBudget, BudgetError> {
    Ok(LoopBudget {
        same_tool_same_args_count_threshold: tighten_u32(
            "same_tool_same_args_count_threshold",
            parent.same_tool_same_args_count_threshold,
            req.same_tool_same_args_count_threshold,
        )?,
        same_tool_same_args_window_turns: tighten_u32(
            "same_tool_same_args_window_turns",
            parent.same_tool_same_args_window_turns,
            req.same_tool_same_args_window_turns,
        )?,
        no_progress_turns_threshold: tighten_u32(
            "no_progress_turns_threshold",
            parent.no_progress_turns_threshold,
            req.no_progress_turns_threshold,
        )?,
        cost_acceleration_ratio: tighten_f32(
            "cost_acceleration_ratio",
            parent.cost_acceleration_ratio,
            req.cost_acceleration_ratio,
        )?,
        cost_acceleration_windows: tighten_u32(
            "cost_acceleration_windows",
            parent.cost_acceleration_windows,
            req.cost_acceleration_windows,
        )?,
        grace_window_turns: tighten_u32(
            "grace_window_turns",
            parent.grace_window_turns,
            req.grace_window_turns,
        )?,
        override_action: tighten_action(parent.override_action, req.override_action)?,
    })
}

fn tighten_u32(field: &'static str, parent: u32, req: Option<u32>) -> Result<u32, BudgetError> {
    match req {
        None => Ok(parent),
        Some(v) if v <= parent => Ok(v),
        Some(v) => Err(BudgetError::RelaxationAttempted {
            field,
            requested: v.to_string(),
            parent: parent.to_string(),
        }),
    }
}

fn tighten_f32(field: &'static str, parent: f32, req: Option<f32>) -> Result<f32, BudgetError> {
    match req {
        None => Ok(parent),
        Some(v) if v <= parent => Ok(v),
        Some(v) => Err(BudgetError::RelaxationAttempted {
            field,
            requested: v.to_string(),
            parent: parent.to_string(),
        }),
    }
}

fn tighten_action(
    parent: OverrideAction,
    req: Option<OverrideAction>,
) -> Result<OverrideAction, BudgetError> {
    match req {
        None => Ok(parent),
        Some(v) if v.severity() >= parent.severity() => Ok(v),
        Some(v) => Err(BudgetError::RelaxationAttempted {
            field: "override_action",
            requested: format!("{v:?}"),
            parent: format!("{parent:?}"),
        }),
    }
}
