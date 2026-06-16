pub mod error;
pub mod engine;
pub mod task;

pub use error::{AutomationError, Result};
pub use engine::{AutomationEngine, EngineConfig};
pub use task::{AutomationTask, TaskStatus, TaskResult};
