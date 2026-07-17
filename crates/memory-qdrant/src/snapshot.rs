//! The snapshot hook: capture a Qdrant collection snapshot and record it as a
//! `MemorySnapshot` event on an external receipt chain.
//!
//! Snapshots make the durable memory point-in-time recoverable; recording the
//! snapshot id on the receipt chain ties that recovery point into the same
//! auditable lineage as every other side effect of a turn. The receipt chain is
//! kept at arm's length behind [`SnapshotReceiptSink`] so this crate does not
//! depend on the concrete receipt type — any sink (including a plain `Vec`) can
//! receive the event.

use serde::{Deserialize, Serialize};

/// A point-in-time Qdrant snapshot, recorded as a receipt-chain event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySnapshot {
    /// The Qdrant snapshot name/id returned by `create_snapshot`.
    pub snapshot_id: String,
    /// Wall-clock instant (ms since the Unix epoch) the snapshot was taken.
    pub ts: u64,
}

/// A receipt chain (or any append-only sink) that a [`MemorySnapshot`] event can
/// be written into. Implemented for `Vec<MemorySnapshot>` so callers and tests
/// can collect events without pulling in the receipt crate.
pub trait SnapshotReceiptSink {
    /// Append a `MemorySnapshot` event to the chain.
    fn append_memory_snapshot(&mut self, snapshot: MemorySnapshot);
}

impl SnapshotReceiptSink for Vec<MemorySnapshot> {
    fn append_memory_snapshot(&mut self, snapshot: MemorySnapshot) {
        self.push(snapshot);
    }
}
