//! ardur-tool-registry — the §6.0 tool layer: the [`Tool`] contract, the
//! [`ToolRegistry`] that resolves and lists tools, and the capability classes
//! the authorization layers gate invocations against.
//!
//! Plan family: §6.0 (`plans/6.0-tool-registry-blueprint.md`).
//!
//! # Phase 1 (this crate)
//!
//! - [`Tool`] — the object-safe trait every tool implements: [`Tool::id`], its
//!   [`Tool::schema`], an async [`Tool::invoke`], and the declarative
//!   [`Tool::required_capabilities`].
//! - [`ToolSchema`] / [`ToolExample`] — the input/output JSON Schemas, the
//!   description, and the prompting examples a tool advertises.
//! - [`ToolContext`] — the authorization handle, owning [`SessionId`], working
//!   directory, environment, and remaining cost budget a call runs against.
//! - [`ToolOutput`] — the tool's result content, its billed [`CostTuple`], and
//!   the data folded into the §11.14 receipt.
//! - [`Capability`] — the resource classes a tool declares it needs.
//! - [`ToolRegistry`] — id→tool resolution keyed by [`ToolId`], with lookup by
//!   id and by required capability.
//! - [`EchoTool`] — a capability-free sample tool that returns its input.
//! - [`ShellTool`] / [`ReadFileTool`] / [`WriteFileTool`] / [`ListDirTool`]
//!   (§6.1) and [`HttpFetchTool`] (§6.2) — capability-gated built-in shell,
//!   filesystem, and SSRF-defended HTTP-fetch tools, installed via
//!   [`ToolRegistry::register_builtins`] and [`BuiltinOpts`]. See
//!   `src/builtins/README.md` for the security model.
//! - [`SkillTool`] / [`SkillLoader`] / [`Skill`] (§8.X) — load filesystem
//!   `SKILL.md` documents (YAML frontmatter + Markdown body) and expose each as
//!   a [`Tool`].
//! - [`ToolError`] / [`RegistryError`] — the crate's typed-error surfaces.
//!
//! [`CapTokenRef`], [`SessionId`], and [`CostTuple`] are re-exported from
//! `ardur-runtime` so the runtime and the tool layer share one schema for the
//! authorizing token, the owning session, and the billed cost — rather than
//! redefining placeholders that would later have to be reconciled.
//!
//! In-process tool execution is the whole of Phase 1; the registry declares
//! capabilities but does not yet enforce them. Phase 2 (see the inline
//! `// TODO §6.0 Phase 2:` markers) adds MCP tool adapters, plugin loading,
//! async streaming output, and sandbox enforcement.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

// TODO §6.0 Phase 2: MCP tool adapters — wrap a remote MCP server's tools as
// `dyn Tool` so external tools register alongside in-process ones.
// TODO §6.0 Phase 2: plugin loading — discover and register tools from
// dynamically loaded plugins rather than only compiled-in ones.
// TODO §6.0 Phase 2: async streaming output — let a long-running tool stream
// incremental `ToolOutput` chunks instead of a single terminal value.
// TODO §6.0 Phase 2: sandbox enforcement — gate `invoke` on the cap-token +
// Cedar verdict for each declared `Capability`, and confine fs/process access.

mod builtins;
mod capability;
mod echo;
mod error;
mod health;
mod mcp;
mod registry;
mod skills;
mod tool;

pub use builtins::{
    BuiltinOpts, HttpFetchOpts, HttpFetchTool, ListDirTool, ReadFileTool, ShellTool, WriteFileTool,
    is_internal_ip,
};
pub use capability::Capability;
pub use echo::EchoTool;
pub use error::{RegistryError, ToolError};
pub use health::HealthCheckTool;
pub use mcp::{
    ArdurMcpServer, MCP_CAPABILITY, RemoteMcpTool, RemoteMcpToolset, bearer_token_allowed,
    extract_bearer_token,
};
pub use registry::ToolRegistry;
pub use skills::{Skill, SkillError, SkillFrontmatter, SkillLoader, SkillTool};
pub use tool::{InvocationId, Tool, ToolContext, ToolExample, ToolId, ToolOutput, ToolSchema};

// Shared value types owned by §1.0; re-exported so the tool layer and the
// runtime never drift into two incompatible schemas.
pub use ardur_runtime::{CapTokenRef, CostTuple, SessionId};
