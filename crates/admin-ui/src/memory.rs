//! Optional, read-only memory queries against the durable Qdrant store.
//!
//! Only wired when `--qdrant-url` is given. We `scroll` the memory collection —
//! a read — and reconstruct each record from the `record_json` payload field
//! the durable store ([`ardur_memory_qdrant`]) writes, reusing its
//! [`MemoryRecord`](ardur_memory_qdrant::MemoryRecord) type so the projection
//! stays in lock-step with the writer. Nothing here mutates Qdrant.

use ardur_memory_qdrant::MemoryRecord;
use qdrant_client::qdrant::ScrollPointsBuilder;
use serde::Serialize;

use crate::state::MemorySource;

/// The `/api/memory/recent` response. When memory is not configured, `enabled`
/// is `false` and `records` is empty.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryRecent {
    /// Whether a Qdrant source is configured.
    pub enabled: bool,
    /// Number of records returned.
    pub count: usize,
    /// The reconstructed records (newest scroll page).
    pub records: Vec<MemoryRecord>,
}

impl MemoryRecent {
    /// The response when no `--qdrant-url` was configured.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            count: 0,
            records: Vec::new(),
        }
    }
}

/// Scroll up to `limit` records from the configured memory collection. A point
/// whose `record_json` cannot be reconstructed is skipped rather than failing
/// the whole read.
pub async fn recent(source: &MemorySource, limit: u32) -> anyhow::Result<MemoryRecent> {
    let response = source
        .client
        .scroll(
            ScrollPointsBuilder::new(&source.collection)
                .limit(limit)
                .with_payload(true)
                .with_vectors(false),
        )
        .await
        .map_err(|e| anyhow::anyhow!("qdrant scroll: {e}"))?;

    let records: Vec<MemoryRecord> = response
        .result
        .into_iter()
        .filter_map(|point| {
            point
                .payload
                .get("record_json")
                .and_then(|v| v.as_str())
                .and_then(|raw| serde_json::from_str::<MemoryRecord>(raw).ok())
        })
        .collect();

    Ok(MemoryRecent {
        enabled: true,
        count: records.len(),
        records,
    })
}
