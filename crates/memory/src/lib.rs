//! ardur-memory — bi-temporal memory substrate.
//!
//! Plan family: §7.0 (`plans/7.0-…` memory / context / sessions / knowledge).
//!
//! The store is bi-temporal: every record carries an *event time* (when the
//! fact happened) and a *valid time* interval (`valid_from` .. `valid_to`),
//! and is invalidated — never deleted — so history is always reconstructable.
//!
//! PHASE 0: contracts only. No implementation bodies — every trait method is
//! `unimplemented!()`. The public trait surface is FROZEN against §7.0;
//! widening it is a §0.0 amendment. Bodies land in §7.0 Phase 1.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::Result;
use chrono::{DateTime, Utc};

/// A reference to the entity a memory record is about (a session, a user, a
/// knowledge node). Opaque string identifier at Phase 0.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EntityRef(pub String);

/// The stable identifier of a single memory record.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RecordId(pub String);

/// A bi-temporal memory record. The four timestamps are the bi-temporal core:
/// `event_time` is when the fact occurred; `valid_from`/`valid_to` bound when
/// the fact is held true; `invalidation_time` records a soft-delete.
pub trait MemoryRecord {
    /// When the underlying fact actually happened.
    fn event_time(&self) -> DateTime<Utc>;
    /// Start of the interval during which this record is held valid.
    fn valid_from(&self) -> DateTime<Utc>;
    /// End of the valid interval, or `None` if still valid.
    fn valid_to(&self) -> Option<DateTime<Utc>>;
    /// When this record was invalidated (soft-deleted), or `None` if live.
    fn invalidation_time(&self) -> Option<DateTime<Utc>>;
}

/// The bi-temporal memory runtime: time-travel reads plus invalidate-not-delete
/// writes.
pub trait MemoryRuntime {
    /// All records whose state was known to the store as of wall-clock `t`.
    fn at_time(&self, t: DateTime<Utc>) -> Vec<Box<dyn MemoryRecord>> {
        let _ = t;
        unimplemented!("Phase 0 contract — body lands in §7.0 Phase 1")
    }
    /// All records held valid as of `t` (the bi-temporal "current" view).
    fn current_as_of(&self, t: DateTime<Utc>) -> Vec<Box<dyn MemoryRecord>> {
        let _ = t;
        unimplemented!("Phase 0 contract — body lands in §7.0 Phase 1")
    }
    /// The full version history of a single entity.
    fn history_of(&self, entity: EntityRef) -> Vec<Box<dyn MemoryRecord>> {
        let _ = entity;
        unimplemented!("Phase 0 contract — body lands in §7.0 Phase 1")
    }
    /// Invalidate (soft-delete) a record as of time `at`. Never removes rows.
    fn invalidate(&mut self, record_id: RecordId, at: DateTime<Utc>) -> Result<()> {
        let _ = (record_id, at);
        unimplemented!("Phase 0 contract — body lands in §7.0 Phase 1")
    }
}
