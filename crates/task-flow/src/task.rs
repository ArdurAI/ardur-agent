use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// Unique identifier for a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub Uuid);

impl TaskId {
    /// Generate a new random task ID.
    pub fn new() -> Self {
        Self(Uuid::new_v7(uuid::Timestamp::now(uuid::NoContext)))
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Status of a task during execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    /// Task is waiting for dependencies.
    Pending,
    /// Task is currently executing.
    Running,
    /// Task completed successfully.
    Completed,
    /// Task failed.
    Failed,
}

impl Default for TaskStatus {
    fn default() -> Self {
        TaskStatus::Pending
    }
}

/// Result of executing a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaskResult {
    /// Task succeeded with an optional value.
    Success(Option<serde_json::Value>),
    /// Task failed with an error message.
    Failure(String),
}

/// A unit of work in the task graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// Unique identifier.
    pub id: TaskId,
    /// Human-readable name.
    pub name: String,
    /// Current status.
    #[serde(skip)]
    pub status: TaskStatus,
    /// Result after execution.
    #[serde(skip)]
    pub result: Option<TaskResult>,
    /// Optional payload for the task executor.
    pub payload: Option<serde_json::Value>,
}

impl Task {
    /// Create a new task with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: TaskId::new(),
            name: name.into(),
            status: TaskStatus::Pending,
            result: None,
            payload: None,
        }
    }

    /// Create a new task with a specific ID.
    pub fn with_id(id: TaskId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            status: TaskStatus::Pending,
            result: None,
            payload: None,
        }
    }

    /// Set the payload for this task.
    pub fn with_payload(mut self, payload: impl Into<serde_json::Value>) -> Self {
        self.payload = Some(payload.into());
        self
    }
}
