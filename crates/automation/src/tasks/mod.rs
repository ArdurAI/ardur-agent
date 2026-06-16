//! Task-flow orchestration contract.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ardur_cap_token::VerifiedClaims;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::TaskFlowError;
use crate::tasks::flow::{
    FlowControl, FlowStep, StepId, StepOutcome, TaskFlowDag, TaskId, TaskOutcome,
};

/// Domain types for declared task-flow DAGs.
pub mod flow;

/// Request to create a task with an optional declared flow DAG.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskCreationRequest {
    /// Human-facing task description.
    pub description: String,
    /// Declared multi-step DAG. `None` keeps the task in single-step mode.
    pub flow_dag: Option<TaskFlowDag>,
}

/// Handle returned after task creation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskHandle {
    /// Stable task id.
    pub task_id: TaskId,
}

/// Runtime state for a task-flow DAG.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskRuntimeState {
    /// The task whose runtime state this is.
    pub task_id: TaskId,
    /// Current root-level outcome if the task has reached a terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<TaskOutcome>,
    /// Step outcomes recorded so far, keyed by step id.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub step_outcomes: HashMap<StepId, StepOutcome>,
    /// Currently active flow control, when the evaluator has entered one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_control: Option<FlowControl>,
    /// Currently active step, when a leaf step is running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_step: Option<FlowStep>,
}

impl TaskRuntimeState {
    /// Create an empty runtime state for a task.
    #[must_use]
    pub fn new(task_id: TaskId) -> Self {
        Self {
            task_id,
            outcome: None,
            step_outcomes: HashMap::new(),
            active_control: None,
            active_step: None,
        }
    }
}

/// Closed orchestration trait for §9.9 task flows.
#[async_trait]
pub trait TaskFlowOrchestrator: Send + Sync + 'static {
    /// Create a task with a declared DAG.
    async fn create_task(
        &self,
        token: &VerifiedClaims,
        request: TaskCreationRequest,
    ) -> Result<TaskHandle, TaskFlowError>;

    /// Cancel an in-flight task DAG-wide. Idempotent in later phases.
    async fn cancel_task(
        &self,
        token: &VerifiedClaims,
        task_id: TaskId,
    ) -> Result<(), TaskFlowError>;

    /// Query the current runtime state of a task.
    async fn get_task_state(&self, task_id: TaskId) -> Result<TaskRuntimeState, TaskFlowError>;

    /// Operator override for a verification-failed step transition.
    async fn operator_override_verification(
        &self,
        token: &VerifiedClaims,
        task_id: TaskId,
        step_id: StepId,
        reason: String,
    ) -> Result<(), TaskFlowError>;
}

/// Production orchestrator placeholder.
///
/// Later phases replace these `NotImplemented` errors with DAG validation,
/// execution, Cedar invariant checks, receipt emission, and persistence.
#[derive(Clone, Debug, Default)]
pub struct DefaultTaskFlowOrchestrator;

#[async_trait]
impl TaskFlowOrchestrator for DefaultTaskFlowOrchestrator {
    async fn create_task(
        &self,
        _token: &VerifiedClaims,
        _request: TaskCreationRequest,
    ) -> Result<TaskHandle, TaskFlowError> {
        Err(TaskFlowError::NotImplemented {
            feature: "DefaultTaskFlowOrchestrator::create_task",
        })
    }

    async fn cancel_task(
        &self,
        _token: &VerifiedClaims,
        _task_id: TaskId,
    ) -> Result<(), TaskFlowError> {
        Err(TaskFlowError::NotImplemented {
            feature: "DefaultTaskFlowOrchestrator::cancel_task",
        })
    }

    async fn get_task_state(&self, _task_id: TaskId) -> Result<TaskRuntimeState, TaskFlowError> {
        Err(TaskFlowError::NotImplemented {
            feature: "DefaultTaskFlowOrchestrator::get_task_state",
        })
    }

    async fn operator_override_verification(
        &self,
        _token: &VerifiedClaims,
        _task_id: TaskId,
        _step_id: StepId,
        _reason: String,
    ) -> Result<(), TaskFlowError> {
        Err(TaskFlowError::NotImplemented {
            feature: "DefaultTaskFlowOrchestrator::operator_override_verification",
        })
    }
}

/// In-memory orchestrator used by tests and downstream contract checks.
#[derive(Clone, Debug, Default)]
pub struct MockTaskFlowOrchestrator {
    states: Arc<Mutex<HashMap<TaskId, TaskRuntimeState>>>,
}

impl MockTaskFlowOrchestrator {
    /// Create a mock orchestrator with no tasks.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl TaskFlowOrchestrator for MockTaskFlowOrchestrator {
    async fn create_task(
        &self,
        _token: &VerifiedClaims,
        request: TaskCreationRequest,
    ) -> Result<TaskHandle, TaskFlowError> {
        if request.description.trim().is_empty() {
            return Err(TaskFlowError::InvalidRequest(
                "description must not be empty".to_string(),
            ));
        }

        let task_id = TaskId::new();
        let mut state = TaskRuntimeState::new(task_id);
        state.active_control = request
            .flow_dag
            .as_ref()
            .and_then(|dag| dag.root.as_control().cloned());
        state.active_step = request
            .flow_dag
            .as_ref()
            .and_then(|dag| dag.root.as_step().cloned());

        self.states
            .lock()
            .expect("mock state lock poisoned")
            .insert(task_id, state);
        Ok(TaskHandle { task_id })
    }

    async fn cancel_task(
        &self,
        _token: &VerifiedClaims,
        task_id: TaskId,
    ) -> Result<(), TaskFlowError> {
        let mut states = self.states.lock().expect("mock state lock poisoned");
        let state = states
            .get_mut(&task_id)
            .ok_or(TaskFlowError::TaskNotFound(task_id))?;
        state.outcome = Some(TaskOutcome::Cancelled);
        state.active_control = None;
        state.active_step = None;
        Ok(())
    }

    async fn get_task_state(&self, task_id: TaskId) -> Result<TaskRuntimeState, TaskFlowError> {
        self.states
            .lock()
            .expect("mock state lock poisoned")
            .get(&task_id)
            .cloned()
            .ok_or(TaskFlowError::TaskNotFound(task_id))
    }

    async fn operator_override_verification(
        &self,
        _token: &VerifiedClaims,
        task_id: TaskId,
        step_id: StepId,
        reason: String,
    ) -> Result<(), TaskFlowError> {
        if reason.trim().is_empty() {
            return Err(TaskFlowError::InvalidRequest(
                "override reason must not be empty".to_string(),
            ));
        }

        let mut states = self.states.lock().expect("mock state lock poisoned");
        let state = states
            .get_mut(&task_id)
            .ok_or(TaskFlowError::TaskNotFound(task_id))?;
        state.step_outcomes.insert(step_id, StepOutcome::Succeeded);
        Ok(())
    }
}
