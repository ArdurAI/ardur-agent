//! Serializable domain model for §9.9 task-flow DAGs.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use ardur_receipt::Sha256Digest;
pub use ardur_runtime::ProviderId;

/// Stable task identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub Uuid);

impl TaskId {
    /// Mint a fresh time-ordered task id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable step identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StepId(pub Uuid);

impl StepId {
    /// Mint a fresh time-ordered step id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for StepId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable invocation identifier for one step attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InvocationId(pub Uuid);

impl InvocationId {
    /// Mint a fresh time-ordered invocation id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for InvocationId {
    fn default() -> Self {
        Self::new()
    }
}

/// SHA-256 hash of a canonical policy/config bundle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BundleHash(pub Sha256Digest);

/// Mission identifier associated with a task.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MissionId(pub String);

/// Capability-token JTI or equivalent verified token id.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapTokenJti(pub String);

/// Cedar decision id recorded on conditional branch receipts.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CedarDecisionId(pub String);

/// Cedar expression serialized from the authoring layer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CedarExpression(pub serde_json::Value);

/// Cedar invariant serialized from the active bundle/overlay chain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CedarInvariant(pub serde_json::Value);

/// Reference to a §11.15 structured-check set.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StructuredCheckSetRef(pub String);

/// Identifier of a tool from the §6 tool registry.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ToolId(pub String);

/// Identifier of an outbound webhook endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EndpointId(pub String);

/// Memory scope a step may write into.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryScope(pub String);

/// Reference to a reusable sub-DAG template.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubDagRef(pub String);

/// Mission phase visible to conditional Cedar predicates.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MissionPhase(pub String);

/// Serializable DAG for a multi-step task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskFlowDag {
    /// Hash of the canonical DAG serialization.
    pub dag_hash: BundleHash,
    /// Schema version.
    pub version: u32,
    /// Root node.
    pub root: FlowNode,
    /// Maximum accepted depth.
    pub max_depth: u32,
    /// Maximum branch count accepted at any parallel node.
    pub max_fanout: u32,
    /// Cedar invariants evaluated at create/transition time in later phases.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invariants: Vec<CedarInvariant>,
    /// Projected total cost in micro-USD.
    pub estimated_total_cost_micro_usd: u64,
}

/// Either a leaf step or an internal flow-control node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum FlowNode {
    /// A single effectful operation.
    Step(FlowStep),
    /// An orchestration primitive.
    Control(FlowControl),
}

impl FlowNode {
    /// Borrow this node as a step, if it is one.
    #[must_use]
    pub fn as_step(&self) -> Option<&FlowStep> {
        match self {
            Self::Step(step) => Some(step),
            Self::Control(_) => None,
        }
    }

    /// Borrow this node as a control, if it is one.
    #[must_use]
    pub fn as_control(&self) -> Option<&FlowControl> {
        match self {
            Self::Step(_) => None,
            Self::Control(control) => Some(control),
        }
    }
}

/// A leaf in the DAG: one effectful operation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FlowStep {
    /// Stable step id.
    pub step_id: StepId,
    /// Human-readable step name.
    pub step_name: String,
    /// Operation this step dispatches.
    pub dispatch: StepDispatch,
    /// Optional structured-check set run after the step completes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_via: Option<StructuredCheckSetRef>,
    /// Projected cost in micro-USD.
    pub estimated_cost_micro_usd: u32,
    /// Projected wall-clock duration in milliseconds.
    pub estimated_duration_ms: u32,
}

/// Effectful operation a step can dispatch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum StepDispatch {
    /// Invoke a registered tool.
    ToolCall {
        /// Tool id.
        tool_id: ToolId,
        /// JSON template for tool arguments.
        args_template: serde_json::Value,
    },
    /// Invoke a model/provider call.
    ProviderCall {
        /// Provider id.
        provider_id: ProviderId,
        /// JSON template for provider request.
        request_template: serde_json::Value,
    },
    /// Emit to an outbound webhook endpoint.
    WebhookEmit {
        /// Endpoint id.
        endpoint_id: EndpointId,
        /// JSON template for webhook payload.
        payload_template: serde_json::Value,
    },
    /// Delegate to a sub-agent.
    SubagentDelegate {
        /// Opaque delegation spec for Phase 1.
        spec: serde_json::Value,
    },
    /// Write a value to memory.
    MemoryWrite {
        /// Memory scope.
        scope: MemoryScope,
        /// JSON template for the value to write.
        value_template: serde_json::Value,
    },
    /// Invoke a reusable sub-DAG template.
    Composite {
        /// Sub-DAG reference.
        sub_dag_ref: SubDagRef,
    },
}

/// Internal orchestration primitive.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum FlowControl {
    /// Run each node in order.
    Sequence(Vec<FlowNode>),
    /// Run branches concurrently and wait according to `wait`.
    Parallel {
        /// Branch roots.
        branches: Vec<FlowNode>,
        /// Wait policy.
        wait: ParallelWait,
    },
    /// Evaluate a Cedar predicate and run the selected branch.
    Conditional {
        /// Cedar predicate.
        predicate: CedarExpression,
        /// Branch used when predicate evaluates true.
        then_branch: Box<FlowNode>,
        /// Optional branch used when predicate evaluates false.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        else_branch: Option<Box<FlowNode>>,
    },
    /// Retry a node according to the policy.
    Retry {
        /// Wrapped step/control.
        step: Box<FlowNode>,
        /// Retry policy.
        policy: RetryPolicy,
    },
}

/// Parallel wait policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParallelWait {
    /// Wait for all branches.
    All,
    /// First completed branch wins.
    Any,
    /// First N completed branches win.
    AnyN(u32),
}

/// Retry policy shared with the outbound webhook surface in later phases.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Maximum number of attempts, including the first attempt.
    pub max_attempts: u32,
    /// Initial backoff delay in milliseconds.
    pub initial_backoff_ms: u64,
    /// Maximum backoff delay in milliseconds.
    pub max_backoff_ms: u64,
    /// Multiplier applied between retry attempts.
    pub backoff_multiplier: u32,
}

/// Context visible to a conditional Cedar predicate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConditionalContext {
    /// Previous step outcome, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_step_outcome: Option<StepOutcome>,
    /// Cost accumulated so far in micro-USD.
    pub cumulative_cost_micro_usd: u64,
    /// Current mission phase.
    pub mission_phase: MissionPhase,
    /// Active tenant overlay hash.
    pub tenant_overlay_hash: BundleHash,
    /// Runtime elapsed for this task in milliseconds.
    pub task_runtime_ms: u64,
    /// Capability token id.
    pub cap_token_jti: CapTokenJti,
}

/// Handle for one parallel branch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParallelBranchHandle {
    /// Zero-based branch index.
    pub branch_index: u32,
    /// Root step id for this branch.
    pub branch_root_step_id: StepId,
    /// Current branch state.
    pub branch_state: BranchState,
}

/// Runtime state of a parallel branch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "data", rename_all = "snake_case")]
pub enum BranchState {
    /// Branch is waiting to start.
    Pending,
    /// Branch is running.
    Running {
        /// Start timestamp in Unix milliseconds.
        started_at_ms: u64,
    },
    /// Branch completed.
    Completed {
        /// Branch outcome.
        outcome: BranchOutcome,
        /// Completion timestamp in Unix milliseconds.
        completed_at_ms: u64,
    },
    /// Branch was cancelled.
    Cancelled,
}

/// Terminal outcome for a parallel branch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchOutcome {
    /// Branch succeeded.
    Succeeded,
    /// Branch failed.
    Failed,
    /// Branch was cancelled.
    Cancelled,
}

/// Invocation metadata for one step attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepInvocation {
    /// Unique invocation id.
    pub invocation_id: InvocationId,
    /// Step being invoked.
    pub step_id: StepId,
    /// Retry attempt, starting at 1.
    pub retry_attempt: u32,
    /// Start timestamp in Unix milliseconds.
    pub started_at_ms: u64,
    /// Dispatch kind.
    pub dispatched_via: DispatchKind,
    /// Hash of the rendered arguments.
    pub args_hash: Sha256Digest,
}

/// Dispatch family for receipt attribution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchKind {
    /// Tool dispatch.
    Tool,
    /// Provider dispatch.
    Provider,
    /// Outbound webhook dispatch.
    Webhook,
    /// Sub-agent delegation.
    Subagent,
    /// Memory write.
    Memory,
    /// Composite sub-DAG.
    Composite,
}

/// Result of one step attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepResult {
    /// Invocation this result belongs to.
    pub invocation_id: InvocationId,
    /// Step outcome.
    pub outcome: StepOutcome,
    /// Hash of the output, when an output exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_hash: Option<Sha256Digest>,
    /// Cost incurred in micro-USD.
    pub cost_micro_usd: u32,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u32,
    /// Completion timestamp in Unix milliseconds.
    pub completed_at_ms: u64,
    /// Optional verification verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_verdict: Option<VerificationVerdict>,
}

/// Outcome of one step attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "data", rename_all = "snake_case")]
pub enum StepOutcome {
    /// Step succeeded.
    Succeeded,
    /// Step failed.
    Failed {
        /// Failure reason.
        failure_kind: StepFailureKind,
    },
    /// Step was cancelled.
    Cancelled,
}

/// Step failure family.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum StepFailureKind {
    /// Policy denied the step.
    PolicyDenied,
    /// Cost envelope was exhausted.
    CostExhausted,
    /// Timeout was exceeded.
    TimeoutExceeded,
    /// Dispatch failed.
    DispatchError(String),
    /// Structured verification failed.
    VerificationFailed,
    /// Retry attempts were exhausted.
    RetryExhausted,
}

/// Structured verification verdict placeholder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationVerdict {
    /// Verification passed.
    Pass,
    /// Verification found a violation.
    Violation,
    /// Evidence was insufficient.
    Insufficient,
}

/// Task-flow-specific receipt body.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum TaskFlowReceiptBody {
    /// Task was created.
    Created {
        /// Task id.
        task_id: TaskId,
        /// DAG hash.
        dag_hash: BundleHash,
        /// Capability token id.
        cap_token_jti: CapTokenJti,
        /// Mission id.
        mission_id: MissionId,
        /// Estimated total cost in micro-USD.
        estimated_total_cost_micro_usd: u64,
    },
    /// Step invocation started.
    StepEntered {
        /// Task id.
        task_id: TaskId,
        /// Step id.
        step_id: StepId,
        /// Invocation id.
        invocation_id: InvocationId,
        /// Retry attempt.
        retry_attempt: u32,
        /// Dispatch kind.
        dispatched_via: DispatchKind,
        /// Cedar decision id for conditional branches.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cedar_decision_id: Option<CedarDecisionId>,
    },
    /// Step invocation completed successfully.
    StepCompleted {
        /// Task id.
        task_id: TaskId,
        /// Step id.
        step_id: StepId,
        /// Invocation id.
        invocation_id: InvocationId,
        /// Output hash.
        output_hash: Sha256Digest,
        /// Cost in micro-USD.
        cost_micro_usd: u32,
        /// Duration in milliseconds.
        duration_ms: u32,
        /// Optional verification verdict.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verification_verdict: Option<VerificationVerdict>,
    },
    /// Step invocation failed.
    StepFailed {
        /// Task id.
        task_id: TaskId,
        /// Step id.
        step_id: StepId,
        /// Invocation id.
        invocation_id: InvocationId,
        /// Failure kind.
        failure_kind: StepFailureKind,
        /// Cost in micro-USD.
        cost_micro_usd: u32,
    },
    /// Flow was halted before normal completion.
    FlowShortCircuited {
        /// Task id.
        task_id: TaskId,
        /// Short-circuit reason.
        reason: ShortCircuitReason,
        /// Last step entered or completed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_step_id: Option<StepId>,
    },
    /// Task reached a terminal state.
    Completed {
        /// Task id.
        task_id: TaskId,
        /// Task outcome.
        outcome: TaskOutcome,
        /// Total cost in micro-USD.
        total_cost_micro_usd: u64,
        /// Total duration in milliseconds.
        total_duration_ms: u64,
        /// Number of entered steps.
        step_count: u32,
    },
}

/// Reason a task flow short-circuited.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShortCircuitReason {
    /// Budget was exhausted.
    BudgetExhausted,
    /// Cedar invariant was violated.
    InvariantViolated,
    /// Task was cancelled.
    Cancelled,
    /// Verification failed.
    VerificationFailed,
    /// Dispatch failed.
    DispatchFailed,
}

/// Terminal task outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    /// Task succeeded.
    Succeeded,
    /// Task failed.
    Failed,
    /// Task was cancelled.
    Cancelled,
}
