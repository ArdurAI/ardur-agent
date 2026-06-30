#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! ardur-llm-helper — LLM task helper with token accounting.
//!
//! Plan family: §6.9 (`plans/6.9-llm-helper-blueprint.md`).

mod error;
mod accounting;
mod tools;

pub use error::{LlmHelperError, Result};
pub use accounting::{TokenAccountant, TaskBudget, UsageRecord};
pub use tools::{LlmTaskTool};
