//! Task persistence model shared by single-step and multi-step automation.

use ardur_receipt::ReceiptBody;
use serde::{Deserialize, Serialize};

use crate::tasks::TaskRuntimeState;
use crate::tasks::flow::{MissionId, TaskFlowDag, TaskId};

/// Persistent task status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// The task has been recorded but has not started.
    Pending,
    /// At least one step or single-step action is running.
    Running,
    /// Cancellation has been requested and in-flight work is draining.
    Cancelling,
    /// The task finished successfully.
    Completed,
    /// The task finished with an error.
    Failed,
    /// The task was cancelled before normal completion.
    Cancelled,
}

/// Persisted record for a unit of unattended work.
///
/// The `flow_*` fields are additive and default to `None` so records written
/// before §9.9 deserialize as single-step tasks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    /// Stable task identifier.
    pub task_id: TaskId,
    /// Mission or workflow this task belongs to.
    pub mission_id: MissionId,
    /// Human-facing task description.
    pub description: String,
    /// Current task status.
    pub status: TaskStatus,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: u64,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: u64,
    /// Receipts already emitted for this task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub receipts: Vec<ReceiptBody>,
    /// Declared multi-step DAG. `None` means pre-§9.9 single-step task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_dag: Option<TaskFlowDag>,
    /// Runtime state for a multi-step task. `None` for single-step tasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_runtime_state: Option<TaskRuntimeState>,
}
