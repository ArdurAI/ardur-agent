//! ardur-sandbox — Sandboxed code execution for Python, JavaScript, and Bash.
//!
//! Plan family: §6.4 (`plans/6.4-sandboxed-execution-blueprint.md`).
//!
//! # Phase 1
//!
//! - [`Sandbox`] — manages isolated execution environments.
//! - [`SandboxTool`] — the `sandbox.exec` tool that runs code in isolation.
//! - Language support: Python, JavaScript (Node), Bash.
//! - Timeout enforcement, resource limits, and escape prevention.
//!
//! All tools are capability-gated via [`Capability::ProcessSpawn`] and
//! [`Capability::Custom("sandbox")`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod sandbox;
mod tools;

pub use error::{Result, SandboxError};
pub use sandbox::{Language, Sandbox, SandboxConfig, SandboxResult};
pub use tools::SandboxExecTool;
