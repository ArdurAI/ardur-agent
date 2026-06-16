//! ardur-task-flow — task orchestration and flow controls with task graph,
//! dependencies, and execution engine.
//!
//! Provides a DAG-based task graph, an async execution engine that respects
//! dependencies, and typed error handling.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod context;
mod engine;
mod error;
mod graph;
mod task;

pub use context::TaskContext;
pub use engine::{ExecutionEngine, TaskExecutor};
pub use error::TaskFlowError;
pub use graph::TaskGraph;
pub use task::{Task, TaskId, TaskResult, TaskStatus};
