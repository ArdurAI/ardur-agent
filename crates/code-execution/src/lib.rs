#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! ardur-code-execution — the §6.7 `code.exec` tool: a script-execution
//! surface that lets a model collapse N tool calls into one dispatch by
//! writing a script instead of emitting N separate tool-use turns.
//!
//! Plan family: §6.7
//! (`plans/6.7-code-execution-tool-call-rpc-blueprint.md`).
//!
//! # Phase 1 (this crate)
//!
//! - [`LanguageAdapter`] — the closed trait one interpreter/runtime backs;
//!   [`BashLanguageAdapter`] and [`PythonLanguageAdapter`] are the two Phase 1
//!   impls, running directly on the local host.
//! - [`CodeExecutionCaveat`] — the operator/cap-token ceiling every request
//!   is attenuated against: a language allowlist, a timeout ceiling, a
//!   tool-callback allowlist, an `expose_stderr` override, and an output-size
//!   ceiling. [`CodeExecutionCaveat::attenuate`] narrows a bare request and
//!   never widens it.
//! - [`CodeExecutionReceipt`] / [`ReceiptKind`] — the `code.exec.{requested,
//!   completed,failed,tool_denied}.v1` receipt family, chained by parent id
//!   into a forest rooted at each dispatch's `Requested` receipt.
//! - [`CodeExecutionTool`] (`code.exec`) — the [`Tool`](ardur_tool_registry::Tool)
//!   impl. Every dispatch scans its captured stdout through
//!   `ardur-injection-defense`'s pattern-based filter before returning it to
//!   the caller, and requires [`Capability::ProcessSpawn`] plus the custom
//!   `code_execution` capability.
//! - [`CodeExecutionRequest`] — the attenuated request shape an adapter runs.
//!
//! # What Phase 2 adds
//!
//! This crate deliberately does not yet implement the full §6.7 surface —
//! see the inline `// TODO §6.7 Phase 2:` markers below and the module docs
//! on [`adapter`] for what is scoped out and why:
//!
//! - Tool-call RPC: the child script cannot yet call back into the tool
//!   registry. `tool_allowlist` is accepted, attenuated, and receipted as a
//!   stated intent, but no `UdsRpcTransport`/`FileRpcTransport` or
//!   per-language stub module exists yet.
//! - Node and Rust adapters (`plans/6.7-code-execution-tool-call-rpc-blueprint.md`
//!   names four languages; this crate ships the two least environment-
//!   dependent ones first).
//! - §6.3 backend-matrix routing (Docker/SSH/Singularity/Modal/Daytona/
//!   Vercel) and §11.5 sandbox-runtime wrapping — neither crate exists in
//!   this workspace yet. Until they land, every dispatch in this crate is
//!   unsandboxed local process execution; see the `adapter` module doc for
//!   the operational implication.
//! - Cap-token-to-caveat projection — [`CodeExecutionCaveat::permissive_default`]
//!   is a development-only stand-in for the caveat this crate should
//!   eventually mint from a verified Biscuit block via `ardur-cap-token`.

// TODO §6.7 Phase 2: `RpcTransport` trait + `UdsRpcTransport`/`FileRpcTransport`
// impls, so a running script can dispatch calls back into the tool registry
// instead of only declaring an intent that this crate denies or no-ops.
// TODO §6.7 Phase 2: `NodeLanguageAdapter` + `RustLanguageAdapter`, and the
// per-mission language-env caching (`uv`/`npm`/`cargo`) the blueprint
// describes for all four adapters.
// TODO §6.7 Phase 2: route every dispatch through the §6.3 backend selector
// and wrap it in a §11.5 sandbox runtime once those crates exist.
// TODO §6.7 Phase 2: mint `CodeExecutionCaveat` from a verified cap-token's
// Biscuit block rather than `permissive_default()`.

mod adapter;
mod caveat;
mod error;
mod receipt;
mod tool;

pub use adapter::{AdapterOutput, BashLanguageAdapter, LanguageAdapter, PythonLanguageAdapter};
pub use caveat::CodeExecutionCaveat;
pub use error::CodeExecutionError;
pub use receipt::{CodeExecutionReceipt, ReceiptKind};
pub use tool::{CodeExecutionRequest, CodeExecutionTool, code_execution_capability};
