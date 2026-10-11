//! Pure scenario grading and reporting, with a paired-home runner for `ardur-rs`.
//!
//! [`home`] dispatches through the paired client and waits for exact run answers.
//! [`transcript`] writes synced, redacted recovery records. Missing evidence is
//! reported separately from assertion failures; grading makes no model calls.
//!
//! The historical `ardur-eval` CLI remains a separate binary that
//! POSTs declarative scenario prompts to a running ardur-server URL and grades
//! the replies against matchers (`contains` / `not_contains` / `regex` /
//! `tool_called` / `cost_under`). Results render as JSON, JUnit XML, or
//! Markdown.
//!
//! The library half exposes the three building blocks so they are unit-testable
//! independently of the binary:
//!
//! - [`scenario`] — the YAML scenario format and its loader.
//! - [`runner`] — the HTTP exchange + the pure [`runner::grade`] function.
//! - [`output`] — report rendering in each format.
//!
//! See the crate README for the assumed server `/chat` contract.

#![warn(missing_docs)]

pub mod home;
pub mod output;
pub mod runner;
pub mod scenario;
pub mod transcript;

pub use output::{Format, Summary};
pub use runner::{Outcome, RunConfig, ScenarioResult, grade, run_scenario};
pub use scenario::{Expected, Scenario, ScenarioError};
