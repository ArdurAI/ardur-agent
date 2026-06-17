use thiserror::Error;

/// Typed errors for the task-flow crate.
#[derive(Error, Debug, Clone)]
pub enum TaskFlowError {
    /// A cycle was detected in the task graph.
    #[error("cycle detected in task graph")]
    CycleDetected,

    /// A task was not found in the graph.
    #[error("task not found: {0}")]
    TaskNotFound(String),

    /// A task failed during execution.
    #[error("task failed: {0}")]
    TaskFailed(String),

    /// A dependency failed, preventing this task from running.
    #[error("dependency failed for task: {0}")]
    DependencyFailed(String),

    /// The engine is already running.
    #[error("execution engine is already running")]
    AlreadyRunning,
}
