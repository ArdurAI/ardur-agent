//! ardur-runtime — the §1.0 runtime foundation: the interactive chat runtime,
//! the typed command bus, and the Session domain type every other crate plugs
//! into.
//!
//! Plan family: §1.0 (`plans/1.0-runtime-foundation-blueprint.md`).
//!
//! # Phase 1 (this crate)
//!
//! - [`ChatRuntime`] / [`InMemoryRuntime`] — submit a batch of [`ChatMessage`]s
//!   ([`SubmitRequest`]) to run one turn ([`SubmitResult`]); the in-memory
//!   runtime echoes the last user message, mints a placeholder [`ReceiptId`],
//!   and charges a zeroed [`CostTuple`].
//! - [`CommandBus`] / [`InMemoryCommandBus`] / [`Command`] — a registry of
//!   named command handlers dispatched against a [`CommandContext`].
//! - [`Session`] — a stable [`SessionId`], the bound [`CapTokenRef`], and the
//!   ordered [`ChatMessage`] history.
//! - [`RuntimeError`] — the crate's single typed-error surface.
//!
//! The newtypes [`CapTokenRef`], [`ReceiptId`], [`ProviderId`], and
//! [`CostTuple`] are local Phase-1 placeholders; the inline
//! `// TODO §1.0 Phase 2:` markers point at the cross-crate re-exports
//! (cap-token, receipt) that replace them once the runtime is wired to its
//! siblings.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod command;
mod error;
mod runtime;
mod session;
mod types;

pub use command::{Command, CommandBus, CommandContext, CommandResult, InMemoryCommandBus};
pub use error::RuntimeError;
pub use runtime::{ChatRuntime, InMemoryRuntime, SubmitRequest, SubmitResult};
pub use session::Session;
pub use types::{
    CapTokenRef, ChatMessage, CostTuple, ProviderId, ReceiptId, Role, SessionId, TurnId,
};
