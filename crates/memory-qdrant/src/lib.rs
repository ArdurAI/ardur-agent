//! ardur-memory-qdrant — a durable, Qdrant-backed [`MemoryRuntime`].
//!
//! Plan family: §7.0 (`plans/7.0-*-blueprint.md`). The §7.0 Phase 1 store
//! ([`ardur_memory::InMemoryMemoryRuntime`]) lives only in process and does not
//! survive a restart. This crate is the first §7.0 Phase 2 *durable* backend: it
//! implements the very same [`MemoryRuntime`] trait against a Qdrant collection,
//! so every bi-temporal record is upserted as a Qdrant point and the memory
//! substrate survives a process — or pod — restart.
//!
//! The seam is unchanged: the fused runtime and the server already accept an
//! `Arc<dyn MemoryRuntime + Send + Sync>`, so selecting this backend
//! (`ARDUR_MEMORY=qdrant`) is a boot-time wiring choice with no call-site edits.
//!
//! # Shape
//!
//! * [`QdrantMemoryConfig`] — connection + collection settings, with `from_env`
//!   and a builder.
//! * [`QdrantPayload`] — the bi-temporal payload schema stored on each point.
//! * [`QdrantMemoryRuntime`] — the [`MemoryRuntime`] implementation, plus the
//!   [`snapshot_into_receipt`](QdrantMemoryRuntime::snapshot_into_receipt) hook.
//!
//! [`MemoryRuntime`]: ardur_memory::MemoryRuntime
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod config;
mod payload;
mod runtime;
mod snapshot;

pub use config::QdrantMemoryConfig;
pub use payload::QdrantPayload;
pub use runtime::QdrantMemoryRuntime;
pub use snapshot::{MemorySnapshot, SnapshotReceiptSink};

// Re-export the trait + core types so a downstream selecting this backend can
// name them without also depending on `ardur-memory` directly.
pub use ardur_memory::{MemoryError, MemoryRecord, MemoryRuntime, Result};
