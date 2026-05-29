//! ardur-session-journals — the §7.10 session-journal layer: the
//! [`SessionJournal`] contract, the [`JournalEntry`] log record, the in-memory
//! and append-only-file backends, and the [`JournalRegistry`] that resolves a
//! journal by [`SessionId`].
//!
//! Plan family: §7.10 (`plans/7.10-session-journals-blueprint.md`).
//!
//! A session journal is the durable, replayable record of everything that
//! happened in one agent session: the user/assistant turns, the tool calls and
//! the content digests they carried, the finalized costs, the checkpoints a
//! resume can start from, and the invalidations that retract an earlier entry.
//! Replaying a journal reconstructs session state; replaying *from* a
//! checkpoint resumes a session without re-reading its whole history.
//!
//! # Phase 1 (this crate)
//!
//! - [`SessionJournal`] — the object-safe async trait every backend implements:
//!   [`append`](SessionJournal::append) (write-then-fsync),
//!   [`replay`](SessionJournal::replay) (full),
//!   [`replay_from`](SessionJournal::replay_from) (resume from a checkpoint),
//!   [`close`](SessionJournal::close), and [`session_id`](SessionJournal::session_id).
//! - [`JournalEntry`] — the serde-tagged log record, one variant per kind of
//!   thing a session records.
//! - [`InMemorySessionJournal`] — the Phase 1 default backend; an in-memory
//!   `Vec` behind a non-poisoning `RwLock`. Tests use it.
//! - [`FileSessionJournal`] — an append-only JSONL file at
//!   `<base_dir>/sessions/<session_id>/journal.jsonl`, one serialized
//!   [`JournalEntry`] per line, with `append` doing file-append + fsync.
//! - [`JournalRegistry`] — [`SessionId`]→journal resolution, with a
//!   get-or-create that returns the existing journal rather than minting a new
//!   one for a session already in flight.
//! - [`EntryId`] — the monotonic, per-session position of an entry; returned by
//!   [`append`](SessionJournal::append) and the cursor
//!   [`replay_from`](SessionJournal::replay_from) resumes after.
//! - [`Sha256Digest`] — a hex-encoded SHA-256 content digest (64 hex chars).
//! - [`ReservationId`] — the cost reservation a [`JournalEntry::CostFinalized`]
//!   settles.
//! - [`JournalError`] / [`RegistryError`] — the crate's typed-error surfaces.
//!
//! [`SessionId`] and [`ReceiptId`] are re-exported from `ardur-runtime`, and
//! [`CostTuple`] / [`CostDelta`] / [`UnixTsMillis`] from `ardur-cost-gate`, so a
//! journal entry and the layers that produce it share one schema rather than
//! redefining placeholders that would later have to be reconciled. [`ToolId`]
//! comes from `ardur-tool-registry`.
//!
//! In-memory and filesystem-JSONL persistence are the whole of Phase 1; the
//! pgvector index, remote-backend sync, encryption-at-rest, and retention
//! policies are Phase 2 (see the inline `// TODO §7.10 Phase 2:` markers).
#![forbid(unsafe_code)]
#![warn(missing_docs)]

// TODO §7.10 Phase 2: pgvector indexing — index entry content for semantic
// recall over a session's history rather than only positional replay.
// TODO §7.10 Phase 2: remote sync — replicate the append-only log to a remote
// backend so a session survives the loss of its originating node.
// TODO §7.10 Phase 2: encryption-at-rest — seal each JSONL line under a
// session-scoped key so the on-disk journal is opaque without the key.
// TODO §7.10 Phase 2: retention policies — compact, truncate-after-checkpoint,
// and expire journals per a configurable retention window.

mod error;
mod file;
mod inmemory;
mod journal;
mod registry;
mod types;

pub use error::{JournalError, RegistryError};
pub use file::FileSessionJournal;
pub use inmemory::InMemorySessionJournal;
pub use journal::SessionJournal;
pub use registry::JournalRegistry;
pub use types::{EntryId, JournalEntry, ReservationId, Sha256Digest};

// Shared value types owned by §1.0 (runtime) and §11.14 (cost-gate), re-exported
// so a journal entry and the layers that produce it share one schema.
pub use ardur_cost_gate::{CostDelta, CostTuple, UnixTsMillis};
pub use ardur_runtime::{ReceiptId, SessionId};
pub use ardur_tool_registry::ToolId;
