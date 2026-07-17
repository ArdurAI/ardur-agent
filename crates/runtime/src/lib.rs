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
//! [`ReceiptId`], [`ProviderId`], and [`CostTuple`] are re-exported from
//! `ardur-core-types`, the workspace's shared primitive crate, so runtime cost
//! accounting shares one type and one schema with the receipt and cost-gate
//! layers. [`CapTokenRef`] remains a local Phase-1 placeholder (see the inline
//! `// TODO §1.0 Phase 2:` marker).
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod command;
mod error;
mod runtime;
mod session;
mod types;

pub use command::{Command, CommandBus, CommandContext, CommandResult, InMemoryCommandBus};
pub use error::{FlagCategory, InjectionFlag, RuntimeError};
pub use runtime::{ChatRuntime, InMemoryRuntime, SubmitRequest, SubmitResult};
pub use session::Session;
// CostTuple, ProviderId, and ReceiptId are owned by `ardur-core-types` and
// re-exported here so existing `ardur_runtime::{CostTuple, ProviderId,
// ReceiptId}` paths keep resolving to the one canonical type.
pub use ardur_core_types::{CostTuple, ProviderId, ReceiptId};
pub use types::{CapTokenRef, ChatMessage, Role, SessionId, ToolCall, TurnId};
