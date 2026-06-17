//! ardur-memory — bi-temporal memory substrate.
//!
//! Plan family: §7.0 (`plans/7.0-*-blueprint.md` memory / context / sessions /
//! knowledge) with the recall/reflection layer in `plans/7.10-*-blueprint.md`.
//!
//! The store is bi-temporal: every record carries an *event time* (when the
//! fact happened) and a *valid time* interval (`valid_from` .. `valid_to`), and
//! is invalidated — never deleted — so history is always reconstructable. See
//! [`MemoryRecord`] for the timestamp model and [`MemoryRuntime`] for the
//! read/write contract.
//!
//! # Phase 1
//!
//! This is the §7.0 Phase 1 landing: the Phase 0 contracts are now backed by a
//! working in-process implementation ([`InMemoryMemoryRuntime`]). It is
//! in-memory only. `// TODO §7.0 Phase 2`: a pgvector-backed durable store,
//! embedding-based recall, and native correction-chain merges replace the flat
//! `Vec` store.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod runtime;
mod types;

pub use error::{MemoryError, Result};
pub use runtime::{InMemoryMemoryRuntime, MemoryRuntime};
pub use types::{
    HolderId, InvalidationReason, MemoryCard, MemoryRecord, ReceiptId, RecordId, RecordKind,
    UnixTsMillis,
};
