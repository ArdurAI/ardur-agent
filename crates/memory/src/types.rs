//! The value types of the bi-temporal memory store.
//!
//! These are deliberately transport-agnostic: a [`MemoryRecord`] is just data.
//! The runtime ([`crate::MemoryRuntime`]) layers time-travel reads and
//! invalidate-not-delete writes on top.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// The holder identity, receipt id, and unix-millis instant are owned by
// `ardur-core-types` and re-exported here so a memory record names the same
// `HolderId`/`ReceiptId`/`UnixTsMillis` the receipt and cost layers do. `Ord`
// on `UnixTsMillis` (defined there) makes the bi-temporal interval comparisons
// total.
pub use ardur_core_types::{HolderId, ReceiptId, UnixTsMillis};

/// The stable identifier of a single memory record. Phase 0 modelled this as a
/// string newtype; Phase 1 makes it a UUIDv4 to match [`MemoryRecord::record_id`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RecordId(pub Uuid);

/// The category of a memory record. Serde-tagged so the kind travels as a
/// self-describing object (`{"kind":"Fact"}`); the record's substance lives in
/// the separate [`MemoryRecord::payload`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum RecordKind {
    /// A durable assertion believed true (e.g. "the user's tz is UTC+1").
    Fact,
    /// A raw, possibly-noisy observation not yet promoted to a fact.
    Observation,
    /// A stated or inferred user preference.
    Preference,
    /// A point-in-time snapshot of ambient context (open files, cwd, etc.).
    ContextSnapshot,
    /// A decision the agent or user made, with its payload as the rationale.
    Decision,
    /// A higher-order reflection synthesised from other records.
    Reflection,
}

/// Why a record was invalidated. Recorded on the appended invalidation record
/// (in its payload) so a correction chain is self-explaining.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InvalidationReason {
    /// A newer record in the same chain replaces this one.
    Superseded,
    /// A human corrected the record.
    UserCorrection,
    /// An external source contradicted the record.
    ExternalContradiction,
    /// The record aged out of its useful window.
    Expired,
}

/// A bi-temporal memory record.
///
/// The timestamps are the bi-temporal core: `event_time` is when the underlying
/// fact actually happened; `valid_from`/`valid_to` bound the interval over which
/// the fact is *held* true; `recorded_at` is when the store learned of it; and
/// `invalidation_time` marks an appended invalidation (records are never
/// mutated or deleted — see [`crate::MemoryRuntime::invalidate`]).
///
/// `correction_chain_root` ties every version of a fact together: a fresh
/// record is the root of its own chain, and an appended invalidation inherits
/// the root so [`crate::MemoryRuntime::history_of`] can reconstruct the lineage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    /// Stable, unique identifier for this record (UUIDv4).
    pub record_id: Uuid,
    /// The holder the record is about.
    pub subject: HolderId,
    /// The category of the record.
    pub kind: RecordKind,
    /// The record's substance — an opaque, kind-specific JSON value.
    pub payload: serde_json::Value,
    /// When the underlying fact actually happened.
    pub event_time: UnixTsMillis,
    /// Start of the interval during which the fact is held valid.
    pub valid_from: UnixTsMillis,
    /// End of the valid interval, or `None` if open-ended.
    pub valid_to: Option<UnixTsMillis>,
    /// Set on an appended invalidation record to mark the cutoff after which
    /// the chain is no longer live; `None` on ordinary data records.
    pub invalidation_time: Option<UnixTsMillis>,
    /// When the store recorded this row (the "transaction time" axis).
    pub recorded_at: UnixTsMillis,
    /// The receipt that authorised the write, if any.
    pub source_receipt_id: Option<ReceiptId>,
    /// The root of this record's correction chain. A brand-new record is its
    /// own root; corrections/invalidations inherit the original's root.
    pub correction_chain_root: Uuid,
}

/// Operator-facing projection of a [`MemoryRecord`].
///
/// A memory card is the shape shown by explorers, exported by CLI/API surfaces,
/// and injected into model context. It keeps the underlying record intact while
/// lifting the trust/provenance fields an operator needs to audit a recall hit:
/// source, workspace/scope, confidence, validity interval, TTL, and the receipt
/// that chained the write.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryCard {
    /// The underlying record id.
    pub record_id: Uuid,
    /// The holder/workspace this memory belongs to.
    pub subject: HolderId,
    /// The record category.
    pub kind: RecordKind,
    /// Human-readable source/provenance label, when supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Workspace/channel/session scope, when supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Confidence score in `0.0..=1.0`, when supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// Start of the held-valid interval.
    pub valid_from: UnixTsMillis,
    /// End of the held-valid interval, if bounded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<UnixTsMillis>,
    /// TTL expiry, if this card should age out independently of `valid_to`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_expires_at: Option<UnixTsMillis>,
    /// Receipt that authorized/chained the write.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt_id: Option<ReceiptId>,
    /// The raw payload for exact audit/export.
    pub payload: serde_json::Value,
}

impl MemoryCard {
    /// Project a stored record into an operator-facing memory card.
    #[must_use]
    pub fn from_record(rec: &MemoryRecord) -> Self {
        Self {
            record_id: rec.record_id,
            subject: rec.subject.clone(),
            kind: rec.kind,
            source: string_payload_field(rec, "source"),
            scope: string_payload_field(rec, "workspace_id")
                .or_else(|| string_payload_field(rec, "scope"))
                .or_else(|| string_payload_field(rec, "session_id"))
                .or_else(|| string_payload_field(rec, "channel_id")),
            confidence: rec
                .payload
                .get("confidence")
                .and_then(serde_json::Value::as_f64),
            valid_from: rec.valid_from,
            valid_to: rec.valid_to,
            ttl_expires_at: rec
                .payload
                .get("ttl_expires_at")
                .and_then(serde_json::Value::as_u64)
                .map(UnixTsMillis),
            receipt_id: rec.source_receipt_id,
            payload: rec.payload.clone(),
        }
    }
}

fn string_payload_field(rec: &MemoryRecord, field: &str) -> Option<String> {
    rec.payload
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

impl MemoryRecord {
    /// Build a fresh, live data record.
    ///
    /// `record_id` and `correction_chain_root` are set to the same new UUIDv4 —
    /// a record with no prior history is the root of its own chain.
    /// `invalidation_time` and `source_receipt_id` start `None`. Use the public
    /// fields directly to attach a receipt or splice into an existing chain.
    pub fn new(
        subject: HolderId,
        kind: RecordKind,
        payload: serde_json::Value,
        event_time: UnixTsMillis,
        valid_from: UnixTsMillis,
        valid_to: Option<UnixTsMillis>,
        recorded_at: UnixTsMillis,
    ) -> Self {
        let record_id = Uuid::new_v4();
        Self {
            record_id,
            subject,
            kind,
            payload,
            event_time,
            valid_from,
            valid_to,
            invalidation_time: None,
            recorded_at,
            source_receipt_id: None,
            correction_chain_root: record_id,
        }
    }
}
