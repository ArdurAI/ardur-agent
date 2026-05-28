//! The bi-temporal memory runtime: time-travel reads and invalidate-not-delete
//! writes.
//!
//! Phase 1 ships a single in-process implementation, [`InMemoryMemoryRuntime`],
//! backed by an append-only `Vec` behind a `parking_lot::RwLock`. There is no
//! persistence and no vector search yet — see the `// TODO §7.0 Phase 2`
//! markers for the pgvector backend, durable storage, and the richer
//! correction-chain semantics that land next.

use std::collections::HashMap;

use parking_lot::RwLock;
use uuid::Uuid;

use crate::error::{MemoryError, Result};
use crate::types::{HolderId, InvalidationReason, MemoryRecord, RecordId, UnixTsMillis};

/// The far end of an open-ended valid interval.
const FOREVER: UnixTsMillis = UnixTsMillis(u64::MAX);

/// Time-travel reads plus invalidate-not-delete writes over a set of
/// [`MemoryRecord`]s.
///
/// Writes are append-only: [`record`](MemoryRuntime::record) adds a row and
/// [`invalidate`](MemoryRuntime::invalidate) appends an invalidation row —
/// neither ever mutates or removes an existing record, so any past state is
/// reconstructable. Reads are infallible (an unknown subject yields an empty
/// `Vec`). All methods take `&self`; implementations use interior mutability so
/// a runtime can be shared across threads behind an `Arc`.
pub trait MemoryRuntime {
    /// Append a record. Returns its [`RecordId`]. Append-only — never mutates
    /// or replaces an existing row.
    fn record(&self, rec: MemoryRecord) -> Result<RecordId>;

    /// All of `subject`'s records held valid at `as_of`.
    ///
    /// A data record is returned when `valid_from <= as_of < valid_to` (an open
    /// `valid_to` reads as "forever") **and** its correction chain has not been
    /// invalidated at or before `as_of`. Invalidation rows themselves are never
    /// returned by this data view.
    ///
    /// Note: the §7.0 sketch states this as a flat per-record predicate
    /// (`… AND (invalidation_time.is_none() OR invalidation_time > as_of)`).
    /// Because invalidation is append-only — the original row is never mutated —
    /// the cutoff is carried by an appended invalidation row and applied at the
    /// chain level here. The flat form is recovered for the common case where a
    /// chain is never invalidated. `// TODO §7.0 Phase 2`: revisit once the
    /// pgvector backend lands correction-chain handling natively.
    fn at_time(&self, subject: &HolderId, as_of: UnixTsMillis) -> Vec<MemoryRecord>;

    /// Sugar for [`at_time`](MemoryRuntime::at_time) at the current wall clock —
    /// the bi-temporal "current" view for `subject`.
    fn current_as_of(&self, subject: &HolderId, now: UnixTsMillis) -> Vec<MemoryRecord> {
        self.at_time(subject, now)
    }

    /// Every record sharing a `correction_chain_root` with the record named by
    /// `record_id`, in insertion order — the full version history of one fact.
    /// Returns an empty `Vec` if `record_id` is unknown.
    fn history_of(&self, record_id: RecordId) -> Vec<MemoryRecord>;

    /// Invalidate the record named by `record_id` as of `at`, for `reason`.
    ///
    /// Appends an invalidation row that inherits the target's
    /// `correction_chain_root` and carries `invalidation_time = Some(at)`; the
    /// original record is left untouched. Returns [`MemoryError::NotFound`] if
    /// `record_id` is unknown.
    fn invalidate(
        &self,
        record_id: RecordId,
        at: UnixTsMillis,
        reason: InvalidationReason,
    ) -> Result<()>;
}

/// The Phase 1 in-process store: an append-only log behind an `RwLock`, with a
/// per-subject index of log positions for fast reads.
///
/// `// TODO §7.0 Phase 2`: swap the `Vec`/`RwLock` for a pgvector-backed,
/// durable store; add embedding-based recall; and formalise correction-chain
/// merges (this Phase 1 store keeps chains as a flat `correction_chain_root`).
pub struct InMemoryMemoryRuntime {
    inner: RwLock<Store>,
}

/// The lock-guarded interior: the append-only log plus its per-subject index.
#[derive(Default)]
struct Store {
    /// Every record ever appended, in insertion order. Indices into this `Vec`
    /// are stable because rows are never removed.
    records: Vec<MemoryRecord>,
    /// `subject -> positions in `records``, so a per-subject read does not scan
    /// the whole log.
    by_subject: HashMap<HolderId, Vec<usize>>,
}

impl InMemoryMemoryRuntime {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for InMemoryMemoryRuntime {
    fn default() -> Self {
        Self {
            inner: RwLock::new(Store::default()),
        }
    }
}

impl MemoryRuntime for InMemoryMemoryRuntime {
    fn record(&self, rec: MemoryRecord) -> Result<RecordId> {
        let id = rec.record_id;
        let subject = rec.subject.clone();
        let mut store = self.inner.write();
        let pos = store.records.len();
        store.records.push(rec);
        store.by_subject.entry(subject).or_default().push(pos);
        Ok(RecordId(id))
    }

    fn at_time(&self, subject: &HolderId, as_of: UnixTsMillis) -> Vec<MemoryRecord> {
        let store = self.inner.read();
        let Some(positions) = store.by_subject.get(subject) else {
            return Vec::new();
        };

        // First pass: the earliest invalidation cutoff per correction chain.
        let mut cutoff: HashMap<Uuid, UnixTsMillis> = HashMap::new();
        for &p in positions {
            let r = &store.records[p];
            if let Some(t) = r.invalidation_time {
                cutoff
                    .entry(r.correction_chain_root)
                    .and_modify(|e| {
                        if t < *e {
                            *e = t;
                        }
                    })
                    .or_insert(t);
            }
        }

        // Second pass: live data rows within their valid interval and not yet
        // cut off by their chain's invalidation.
        positions
            .iter()
            .map(|&p| &store.records[p])
            .filter(|r| r.invalidation_time.is_none())
            .filter(|r| r.valid_from <= as_of && as_of < r.valid_to.unwrap_or(FOREVER))
            .filter(|r| match cutoff.get(&r.correction_chain_root) {
                Some(cut) => *cut > as_of,
                None => true,
            })
            .cloned()
            .collect()
    }

    fn history_of(&self, record_id: RecordId) -> Vec<MemoryRecord> {
        let store = self.inner.read();
        let Some(root) = store
            .records
            .iter()
            .find(|r| r.record_id == record_id.0)
            .map(|r| r.correction_chain_root)
        else {
            return Vec::new();
        };
        store
            .records
            .iter()
            .filter(|r| r.correction_chain_root == root)
            .cloned()
            .collect()
    }

    fn invalidate(
        &self,
        record_id: RecordId,
        at: UnixTsMillis,
        reason: InvalidationReason,
    ) -> Result<()> {
        let mut store = self.inner.write();
        let target = store
            .records
            .iter()
            .find(|r| r.record_id == record_id.0)
            .cloned()
            .ok_or(MemoryError::NotFound(record_id.0))?;

        let tombstone = MemoryRecord {
            record_id: Uuid::new_v4(),
            subject: target.subject.clone(),
            kind: target.kind,
            payload: serde_json::json!({
                "invalidates": target.record_id,
                "reason": reason,
            }),
            event_time: at,
            valid_from: at,
            valid_to: None,
            invalidation_time: Some(at),
            recorded_at: at,
            source_receipt_id: target.source_receipt_id,
            correction_chain_root: target.correction_chain_root,
        };

        let pos = store.records.len();
        let subject = tombstone.subject.clone();
        store.records.push(tombstone);
        store.by_subject.entry(subject).or_default().push(pos);
        Ok(())
    }
}
