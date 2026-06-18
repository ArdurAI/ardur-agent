//! Task-flow orchestration contract.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ardur_cap_token::VerifiedClaims;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::TaskFlowError;
use crate::tasks::flow::{
    FlowControl, FlowNode, FlowStep, ParallelWait, RetryPolicy, StepDispatch, StepFailureKind,
    StepId, StepOutcome, TaskFlowDag, TaskId, TaskOutcome,
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

/// In-memory production-default task-flow orchestrator.
///
/// This deterministic executor validates a declared DAG and records each leaf
/// step's outcome in memory. It deliberately does not dispatch external tools,
/// providers, webhooks, or memory writes yet; instead it exercises the real DAG
/// traversal, timeout/error paths, cancel semantics, and state persistence seam
/// that downstream runtime wiring depends on.
#[derive(Clone, Debug)]
pub struct DefaultTaskFlowOrchestrator {
    states: Arc<Mutex<HashMap<TaskId, TaskRuntimeState>>>,
    step_timeout_ms: u32,
}

impl Default for DefaultTaskFlowOrchestrator {
    fn default() -> Self {
        Self {
            states: Arc::new(Mutex::new(HashMap::new())),
            step_timeout_ms: 30_000,
        }
    }
}

impl DefaultTaskFlowOrchestrator {
    /// Create an in-memory orchestrator using the default 30-second step timeout.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the synthetic per-step timeout used by the deterministic
    /// executor. A `0` value is ignored so the orchestrator never creates a
    /// configuration that times out every step by construction.
    #[must_use]
    pub fn with_step_timeout_ms(mut self, timeout_ms: u32) -> Self {
        if timeout_ms > 0 {
            self.step_timeout_ms = timeout_ms;
        }
        self
    }
}

#[async_trait]
impl TaskFlowOrchestrator for DefaultTaskFlowOrchestrator {
    async fn create_task(
        &self,
        token: &VerifiedClaims,
        request: TaskCreationRequest,
    ) -> Result<TaskHandle, TaskFlowError> {
        validate_creation_request(token, &request)?;
        if let Some(dag) = &request.flow_dag {
            validate_dag(token, dag)?;
        }

        let task_id = TaskId::new();
        let mut state = TaskRuntimeState::new(task_id);
        if let Some(dag) = &request.flow_dag {
            state.active_control = dag.root.as_control().cloned();
            state.active_step = dag.root.as_step().cloned();
            let succeeded = execute_node(&dag.root, &mut state, self.step_timeout_ms);
            state.outcome = Some(if succeeded {
                TaskOutcome::Succeeded
            } else {
                TaskOutcome::Failed
            });
            state.active_control = None;
            state.active_step = None;
        }

        self.states
            .lock()
            .map_err(|_| TaskFlowError::InvalidRequest("task state lock poisoned".to_string()))?
            .insert(task_id, state);
        Ok(TaskHandle { task_id })
    }

    async fn cancel_task(
        &self,
        _token: &VerifiedClaims,
        task_id: TaskId,
    ) -> Result<(), TaskFlowError> {
        let mut states = self
            .states
            .lock()
            .map_err(|_| TaskFlowError::InvalidRequest("task state lock poisoned".to_string()))?;
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
            .map_err(|_| TaskFlowError::InvalidRequest("task state lock poisoned".to_string()))?
            .get(&task_id)
            .cloned()
            .ok_or(TaskFlowError::TaskNotFound(task_id))
    }

    async fn operator_override_verification(
        &self,
        token: &VerifiedClaims,
        task_id: TaskId,
        step_id: StepId,
        reason: String,
    ) -> Result<(), TaskFlowError> {
        if reason.trim().is_empty() {
            return Err(TaskFlowError::InvalidRequest(
                "override reason must not be empty".to_string(),
            ));
        }
        if !token
            .tool_allowlist
            .iter()
            .any(|allowed| allowed == "task.override")
        {
            return Err(TaskFlowError::InvalidRequest(
                "task.override not in cap-token allowlist".to_string(),
            ));
        }
        let mut states = self
            .states
            .lock()
            .map_err(|_| TaskFlowError::InvalidRequest("task state lock poisoned".to_string()))?;
        let state = states
            .get_mut(&task_id)
            .ok_or(TaskFlowError::TaskNotFound(task_id))?;
        match state.step_outcomes.get(&step_id) {
            Some(StepOutcome::Failed {
                failure_kind: StepFailureKind::VerificationFailed,
            }) => {}
            Some(_) => {
                return Err(TaskFlowError::InvalidRequest(
                    "operator override is only valid for verification-failed steps".to_string(),
                ));
            }
            None => {
                return Err(TaskFlowError::InvalidRequest(format!(
                    "step {step_id:?} is not part of the recorded task state"
                )));
            }
        }
        state.step_outcomes.insert(step_id, StepOutcome::Succeeded);
        if state
            .step_outcomes
            .values()
            .all(|outcome| matches!(outcome, StepOutcome::Succeeded))
        {
            state.outcome = Some(TaskOutcome::Succeeded);
        }
        Ok(())
    }
}

fn validate_creation_request(
    token: &VerifiedClaims,
    request: &TaskCreationRequest,
) -> Result<(), TaskFlowError> {
    if request.description.trim().is_empty() {
        return Err(TaskFlowError::InvalidRequest(
            "description must not be empty".to_string(),
        ));
    }
    if token.tool_allowlist.is_empty() {
        return Err(TaskFlowError::InvalidRequest(
            "cap-token tool allowlist must not be empty".to_string(),
        ));
    }
    Ok(())
}

fn validate_dag(token: &VerifiedClaims, dag: &TaskFlowDag) -> Result<(), TaskFlowError> {
    if dag.version == 0 {
        return Err(TaskFlowError::InvalidRequest(
            "DAG version must be non-zero".to_string(),
        ));
    }
    if !dag.invariants.is_empty() {
        return Err(TaskFlowError::InvalidRequest(
            "unsupported Cedar invariants in default orchestrator".to_string(),
        ));
    }
    let depth = node_depth(&dag.root);
    if depth > dag.max_depth {
        return Err(TaskFlowError::InvalidRequest(format!(
            "DAG depth {depth} exceeds max_depth {}",
            dag.max_depth
        )));
    }
    let fanout = max_fanout(&dag.root);
    if fanout > dag.max_fanout {
        return Err(TaskFlowError::InvalidRequest(format!(
            "DAG fanout {fanout} exceeds max_fanout {}",
            dag.max_fanout
        )));
    }
    validate_node(token, &dag.root)
}

fn validate_node(token: &VerifiedClaims, node: &FlowNode) -> Result<(), TaskFlowError> {
    match node {
        FlowNode::Step(step) => validate_step_dispatch(token, step),
        FlowNode::Control(FlowControl::Sequence(nodes)) => {
            if nodes.is_empty() {
                return Err(TaskFlowError::InvalidRequest(
                    "empty sequence control is unsupported".to_string(),
                ));
            }
            for child in nodes {
                validate_node(token, child)?;
            }
            Ok(())
        }
        FlowNode::Control(FlowControl::Parallel { branches, wait }) => {
            if branches.is_empty() {
                return Err(TaskFlowError::InvalidRequest(
                    "empty parallel control is unsupported".to_string(),
                ));
            }
            if let ParallelWait::AnyN(n) = wait {
                if *n == 0 || *n > branches.len() as u32 {
                    return Err(TaskFlowError::InvalidRequest(format!(
                        "AnyN wait must be between 1 and branch count {}, got {n}",
                        branches.len()
                    )));
                }
            }
            for branch in branches {
                validate_node(token, branch)?;
            }
            Ok(())
        }
        FlowNode::Control(FlowControl::Conditional { .. }) => Err(TaskFlowError::InvalidRequest(
            "unsupported conditional control requires Cedar predicate evaluation".to_string(),
        )),
        FlowNode::Control(FlowControl::Retry { step, policy }) => {
            if policy.max_attempts == 0 {
                return Err(TaskFlowError::InvalidRequest(
                    "retry max_attempts must be non-zero".to_string(),
                ));
            }
            validate_node(token, step)
        }
    }
}

fn validate_step_dispatch(token: &VerifiedClaims, step: &FlowStep) -> Result<(), TaskFlowError> {
    if step.step_name.trim().is_empty() {
        return Err(TaskFlowError::InvalidRequest(
            "step name must not be empty".to_string(),
        ));
    }
    let required = dispatch_allowlist_key(&step.dispatch);
    if !token
        .tool_allowlist
        .iter()
        .any(|allowed| allowed == &required)
    {
        return Err(TaskFlowError::InvalidRequest(format!(
            "dispatch `{required}` not in cap-token allowlist"
        )));
    }
    Ok(())
}

fn dispatch_allowlist_key(dispatch: &StepDispatch) -> String {
    match dispatch {
        StepDispatch::ToolCall { tool_id, .. } => tool_id.0.clone(),
        StepDispatch::ProviderCall { provider_id, .. } => format!("provider.{}", provider_id.0),
        StepDispatch::WebhookEmit { endpoint_id, .. } => format!("webhook.{}", endpoint_id.0),
        StepDispatch::SubagentDelegate { .. } => "subagent.delegate".to_string(),
        StepDispatch::MemoryWrite { scope, .. } => format!("memory.{}", scope.0),
        StepDispatch::Composite { sub_dag_ref } => format!("composite.{}", sub_dag_ref.0),
    }
}

fn node_depth(node: &FlowNode) -> u32 {
    match node {
        FlowNode::Step(_) => 1,
        FlowNode::Control(control) => {
            1 + control_children(control)
                .iter()
                .map(|child| node_depth(child))
                .max()
                .unwrap_or(0)
        }
    }
}

fn max_fanout(node: &FlowNode) -> u32 {
    match node {
        FlowNode::Step(_) => 1,
        FlowNode::Control(control) => {
            let own = match control {
                FlowControl::Parallel { branches, .. } | FlowControl::Sequence(branches) => {
                    branches.len() as u32
                }
                FlowControl::Conditional { else_branch, .. } => {
                    1 + u32::from(else_branch.is_some())
                }
                FlowControl::Retry { .. } => 1,
            };
            own.max(
                control_children(control)
                    .iter()
                    .map(|child| max_fanout(child))
                    .max()
                    .unwrap_or(0),
            )
        }
    }
}

fn control_children(control: &FlowControl) -> Vec<&FlowNode> {
    match control {
        FlowControl::Sequence(nodes) => nodes.iter().collect(),
        FlowControl::Parallel { branches, .. } => branches.iter().collect(),
        FlowControl::Conditional {
            then_branch,
            else_branch,
            ..
        } => {
            let mut children = vec![then_branch.as_ref()];
            if let Some(else_branch) = else_branch {
                children.push(else_branch.as_ref());
            }
            children
        }
        FlowControl::Retry { step, .. } => vec![step.as_ref()],
    }
}

fn execute_node(node: &FlowNode, state: &mut TaskRuntimeState, timeout_ms: u32) -> bool {
    match node {
        FlowNode::Step(step) => execute_step(step, state, timeout_ms),
        FlowNode::Control(FlowControl::Sequence(nodes)) => {
            for child in nodes {
                if !execute_node(child, state, timeout_ms) {
                    return false;
                }
            }
            true
        }
        FlowNode::Control(FlowControl::Parallel { branches, wait }) => {
            execute_parallel(branches, wait, state, timeout_ms)
        }
        FlowNode::Control(FlowControl::Conditional { then_branch, .. }) => {
            execute_node(then_branch, state, timeout_ms)
        }
        FlowNode::Control(FlowControl::Retry { step, policy }) => {
            execute_retry(step, policy, state, timeout_ms)
        }
    }
}

fn execute_parallel(
    branches: &[FlowNode],
    wait: &ParallelWait,
    state: &mut TaskRuntimeState,
    timeout_ms: u32,
) -> bool {
    let successes = branches
        .iter()
        .filter(|branch| execute_node(branch, state, timeout_ms))
        .count() as u32;
    match wait {
        ParallelWait::All => successes == branches.len() as u32,
        ParallelWait::Any => successes > 0,
        ParallelWait::AnyN(n) => successes >= *n,
    }
}

fn execute_retry(
    step: &FlowNode,
    policy: &RetryPolicy,
    state: &mut TaskRuntimeState,
    timeout_ms: u32,
) -> bool {
    let attempts = policy.max_attempts.max(1);
    for _ in 0..attempts {
        if execute_node(step, state, timeout_ms) {
            return true;
        }
    }
    false
}

fn execute_step(step: &FlowStep, state: &mut TaskRuntimeState, timeout_ms: u32) -> bool {
    state.active_step = Some(step.clone());
    if step.estimated_duration_ms > timeout_ms {
        state.step_outcomes.insert(
            step.step_id,
            StepOutcome::Failed {
                failure_kind: StepFailureKind::TimeoutExceeded,
            },
        );
        state.active_step = None;
        return false;
    }
    state
        .step_outcomes
        .insert(step.step_id, StepOutcome::Succeeded);
    state.active_step = None;
    true
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
