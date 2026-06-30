//! Error surface for the task-flow contract.

use crate::tasks::flow::TaskId;

/// Errors raised by task-flow orchestration.
#[derive(Debug, thiserror::Error)]
pub enum TaskFlowError {
    /// The production execution path has not landed yet.
    #[error("{feature} is not implemented in §9.9 Phase 1")]
    NotImplemented {
        /// Name of the missing feature.
        feature: &'static str,
    },
    /// A requested task id is unknown to the orchestrator.
    #[error("task not found: {0:?}")]
    TaskNotFound(TaskId),
    /// The caller supplied an invalid task-creation request.
    #[error("invalid task request: {0}")]
    InvalidRequest(String),
}
