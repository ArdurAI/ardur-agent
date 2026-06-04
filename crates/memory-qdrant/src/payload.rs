//! [`QdrantPayload`] — the bi-temporal payload schema stored alongside each
//! Qdrant point.
//!
//! A Qdrant point carries (id, vector, payload). The vector is the embedding of
//! the fact's text (`predicate + object`); this struct is the payload. It pins
//! the bi-temporal fields out at the top level so they are filterable /
//! inspectable in Qdrant, and carries the *full* [`MemoryRecord`] as a JSON
//! string in [`record_json`](QdrantPayload::record_json) so a read reconstructs
//! the original record byte-for-byte — the structured top-level fields are a
//! lossy projection; `record_json` is the source of truth.

use ardur_memory::{MemoryRecord, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The stored payload for one bi-temporal memory record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QdrantPayload {
    /// The record's stable id (`MemoryRecord::record_id`).
    pub fact_id: Uuid,
    /// The holder the record is about (`MemoryRecord::subject`). Payload-indexed.
    pub subject: String,
    /// The fact's predicate, lifted from `payload.predicate` when present.
    pub predicate: Option<String>,
    /// The fact's object, lifted from `payload.object` when present.
    pub object: Option<serde_json::Value>,
    /// When the underlying fact happened (`event_time`, ms since epoch).
    pub event_time: u64,
    /// Start of the valid interval (ms since epoch).
    pub valid_from: u64,
    /// End of the valid interval, or `None` if open-ended.
    pub valid_to: Option<u64>,
    /// The invalidation cutoff on an appended tombstone; `None` on data rows.
    pub invalidation_time: Option<u64>,
    /// Originating channel, lifted from `payload.channel_id` when present.
    /// Payload-indexed. Omitted from the wire form when absent.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub channel_id: Option<String>,
    /// Originating session, lifted from `payload.session_id` when present.
    /// Payload-indexed. Omitted from the wire form when absent.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_id: Option<String>,
    /// The record's correction-chain root — the key a `history_of` read filters
    /// on to gather every version of one fact.
    pub correction_chain_root: Uuid,
    /// The full, lossless [`MemoryRecord`] as a JSON string. The read path
    /// reconstructs the record from this field alone, so a backend round-trip is
    /// exact regardless of the structured projection above.
    pub record_json: String,
}

impl QdrantPayload {
    /// Project a [`MemoryRecord`] into its stored payload, lifting the optional
    /// `predicate` / `object` / `channel_id` / `session_id` out of the record's
    /// opaque JSON payload when they are present.
    ///
    /// # Errors
    /// [`ardur_memory::MemoryError::Backend`] if the record cannot be serialized
    /// to JSON (it always can in practice — the payload is already a
    /// `serde_json::Value`).
    pub fn from_record(rec: &MemoryRecord) -> Result<Self> {
        let record_json = serde_json::to_string(rec)
            .map_err(|e| ardur_memory::MemoryError::Backend(format!("serialize record: {e}")))?;
        Ok(Self {
            fact_id: rec.record_id,
            subject: rec.subject.0.clone(),
            predicate: str_field(rec, "predicate"),
            object: rec.payload.get("object").cloned(),
            event_time: rec.event_time.0,
            valid_from: rec.valid_from.0,
            valid_to: rec.valid_to.map(|t| t.0),
            invalidation_time: rec.invalidation_time.map(|t| t.0),
            channel_id: str_field(rec, "channel_id"),
            session_id: str_field(rec, "session_id"),
            correction_chain_root: rec.correction_chain_root,
            record_json,
        })
    }

    /// Reconstruct the original [`MemoryRecord`] from [`record_json`].
    ///
    /// [`record_json`]: QdrantPayload::record_json
    ///
    /// # Errors
    /// [`ardur_memory::MemoryError::Backend`] if `record_json` is not a valid
    /// serialized record.
    pub fn into_record(&self) -> Result<MemoryRecord> {
        serde_json::from_str(&self.record_json)
            .map_err(|e| ardur_memory::MemoryError::Backend(format!("deserialize record: {e}")))
    }
}

/// Pull a string-valued key out of a record's opaque JSON payload, if present.
fn str_field(rec: &MemoryRecord, key: &str) -> Option<String> {
    rec.payload
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// The natural-language text a record is embedded and lexically-indexed on.
///
/// Hybrid retrieval needs *one* string per record to feed both the dense
/// embedder and the sparse BM25 index. We prefer the structured fact form —
/// `predicate object` lifted out of the payload — because that is the semantic
/// core of a memory; when neither is present (a free-form observation, a
/// tombstone), we fall back to the payload rendered as text so the record is
/// still searchable. A bare JSON string payload is used verbatim (no enclosing
/// quotes), and any other JSON shape is rendered with `to_string`.
#[must_use]
pub fn searchable_text(rec: &MemoryRecord) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(predicate) = rec.payload.get("predicate").and_then(|v| v.as_str()) {
        parts.push(predicate.to_string());
    }
    if let Some(object) = rec.payload.get("object") {
        parts.push(render(object));
    }
    if parts.is_empty() {
        render(&rec.payload)
    } else {
        parts.join(" ")
    }
}

/// Render a JSON value as plain text: a string verbatim, anything else via
/// `to_string` (so an object/number/array still contributes searchable tokens).
fn render(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ardur_memory::{HolderId, RecordKind, UnixTsMillis};

    fn sample_record() -> MemoryRecord {
        MemoryRecord::new(
            HolderId::from("user:payload-test"),
            RecordKind::Fact,
            serde_json::json!({
                "predicate": "prefers",
                "object": "coffee",
                "channel_id": "C123",
                "session_id": "S456",
            }),
            UnixTsMillis(1_000),
            UnixTsMillis(1_000),
            Some(UnixTsMillis(9_000)),
            UnixTsMillis(1_000),
        )
    }

    #[test]
    fn projects_the_bitemporal_fields_and_lifts_optional_keys() {
        let rec = sample_record();
        let payload = QdrantPayload::from_record(&rec).expect("projects");
        assert_eq!(payload.fact_id, rec.record_id);
        assert_eq!(payload.subject, "user:payload-test");
        assert_eq!(payload.predicate.as_deref(), Some("prefers"));
        assert_eq!(payload.object, Some(serde_json::json!("coffee")));
        assert_eq!(payload.event_time, 1_000);
        assert_eq!(payload.valid_from, 1_000);
        assert_eq!(payload.valid_to, Some(9_000));
        assert_eq!(payload.invalidation_time, None);
        assert_eq!(payload.channel_id.as_deref(), Some("C123"));
        assert_eq!(payload.session_id.as_deref(), Some("S456"));
        assert_eq!(payload.correction_chain_root, rec.correction_chain_root);
    }

    #[test]
    fn serde_round_trips_through_json_and_reconstructs_the_record() {
        let rec = sample_record();
        let payload = QdrantPayload::from_record(&rec).expect("projects");

        // Struct ↔ JSON round-trip.
        let json = serde_json::to_value(&payload).expect("serializes");
        let back: QdrantPayload = serde_json::from_value(json).expect("deserializes");
        assert_eq!(back, payload);

        // And the carried record reconstructs byte-for-byte.
        let reconstructed = back.into_record().expect("reconstructs");
        assert_eq!(reconstructed, rec);
    }

    #[test]
    fn optional_keys_absent_are_omitted_from_the_wire_form() {
        let rec = MemoryRecord::new(
            HolderId::from("user:bare"),
            RecordKind::Observation,
            serde_json::json!({ "note": "no predicate here" }),
            UnixTsMillis(5),
            UnixTsMillis(5),
            None,
            UnixTsMillis(5),
        );
        let payload = QdrantPayload::from_record(&rec).expect("projects");
        assert_eq!(payload.predicate, None);
        assert_eq!(payload.channel_id, None);
        assert_eq!(payload.session_id, None);

        let json = serde_json::to_value(&payload).expect("serializes");
        assert!(
            json.get("channel_id").is_none(),
            "absent channel_id is omitted"
        );
        assert!(
            json.get("session_id").is_none(),
            "absent session_id is omitted"
        );

        // Round-trip still reconstructs the record exactly.
        let back: QdrantPayload = serde_json::from_value(json).expect("deserializes");
        assert_eq!(back.into_record().expect("reconstructs"), rec);
    }

    #[test]
    fn searchable_text_prefers_predicate_and_object() {
        let rec = sample_record();
        // predicate "prefers" + object "coffee".
        assert_eq!(searchable_text(&rec), "prefers coffee");
    }

    #[test]
    fn searchable_text_falls_back_to_payload_when_unstructured() {
        // A bare string payload is used verbatim (no JSON quotes).
        let s = MemoryRecord::new(
            HolderId::from("user:s"),
            RecordKind::Observation,
            serde_json::json!("the deploy finished at noon"),
            UnixTsMillis(1),
            UnixTsMillis(1),
            None,
            UnixTsMillis(1),
        );
        assert_eq!(searchable_text(&s), "the deploy finished at noon");

        // An object payload with no predicate/object renders as JSON text.
        let o = MemoryRecord::new(
            HolderId::from("user:o"),
            RecordKind::Observation,
            serde_json::json!({ "note": "no predicate here" }),
            UnixTsMillis(1),
            UnixTsMillis(1),
            None,
            UnixTsMillis(1),
        );
        let text = searchable_text(&o);
        assert!(text.contains("note") && text.contains("no predicate here"));
    }

    #[test]
    fn searchable_text_renders_non_string_object() {
        let rec = MemoryRecord::new(
            HolderId::from("user:num"),
            RecordKind::Fact,
            serde_json::json!({ "predicate": "age", "object": 42 }),
            UnixTsMillis(1),
            UnixTsMillis(1),
            None,
            UnixTsMillis(1),
        );
        assert_eq!(searchable_text(&rec), "age 42");
    }
}
