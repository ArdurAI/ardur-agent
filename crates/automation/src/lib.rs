//! ardur-automation — the §9.x automation substrate contract.
//!
//! Plan families: §9.0 automation / cron / hooks / webhooks foundation and
//! §9.9 task-flow controls. This crate starts with the §9.9 Phase 1 contract
//! surface because the current codebase did not yet contain the §9.0
//! `crates/automation` substrate that the plan expected.
//!
//! # Phase 1 (this crate)
//!
//! - [`TaskRecord`] — a minimal task persistence shape with additive
//!   `flow_dag` and `flow_runtime_state` fields. Plain single-step records load
//!   with both fields as `None`, matching the pre-§9.9 model described by the
//!   plan.
//! - [`TaskFlowDag`], [`FlowStep`], and [`FlowControl`] — the serializable
//!   declared DAG model for multi-step tasks.
//! - [`TaskFlowOrchestrator`] — the closed orchestration trait. The production
//!   implementation is intentionally not present in Phase 1; later phases add
//!   DAG validation, execution, Cedar invariant checks, receipt emission, CRUD,
//!   persistence, and daemon restart resumption.
//! - [`MockTaskFlowOrchestrator`] — an in-memory test implementation that lets
//!   downstream crates compile against the contract without pretending the
//!   execution engine exists.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
pub mod learning;
pub mod proactive;
mod task_record;
pub mod tasks;

pub use error::TaskFlowError;
pub use proactive::{
    AutomationAttenuation, AutomationChannel, AutomationDeliveryEvent, AutomationRuntime,
    AutomationSchedule, AutomationScheduleId, AutomationScheduleStatus, FileScheduleStore,
    FireReport, FusedAutomationRuntime, InMemoryScheduleStore, MpscAutomationChannel,
    ProactiveAutomationError, ProactiveAutomationLoop, ScheduleStore, ScheduledCapToken,
};
pub use task_record::{TaskRecord, TaskStatus};
pub use tasks::flow::{
    BranchOutcome, BranchState, BundleHash, CapTokenJti, CedarDecisionId, CedarExpression,
    CedarInvariant, ConditionalContext, DispatchKind, EndpointId, FlowControl, FlowNode, FlowStep,
    InvocationId, MemoryScope, MissionId, MissionPhase, ParallelBranchHandle, ParallelWait,
    ProviderId, RetryPolicy, Sha256Digest, ShortCircuitReason, StepDispatch, StepFailureKind,
    StepId, StepInvocation, StepOutcome, StepResult, StructuredCheckSetRef, SubDagRef, TaskFlowDag,
    TaskFlowReceiptBody, TaskId, TaskOutcome, ToolId, VerificationVerdict,
};
pub use tasks::{
    DefaultTaskFlowOrchestrator, MockTaskFlowOrchestrator, TaskCreationRequest,
    TaskFlowOrchestrator, TaskHandle, TaskRuntimeState,
};
