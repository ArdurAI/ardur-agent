//! [`QdrantMemoryRuntime`] — the durable, Qdrant-backed [`MemoryRuntime`].
//!
//! Every bi-temporal record is upserted as a Qdrant point (id = the record's
//! UUID, vector = the real embedding of its [`searchable_text`] when an
//! [`Embedder`] is attached — else a placeholder — payload = [`QdrantPayload`]).
//! The bi-temporal `at_time`/`history_of` reads scroll by payload filter and
//! apply the *same* "as-of" predicate the in-process store uses, so those read
//! semantics are identical; vector *search* ([`search_vectors`]) is the new dense
//! recall surface the hybrid retriever fuses with BM25.
//!
//! [`Embedder`]: ardur_embeddings::Embedder
//! [`searchable_text`]: crate::searchable_text
//! [`search_vectors`]: QdrantMemoryRuntime::search_vectors
//!
//! ## Sync trait over an async client
//!
//! [`MemoryRuntime`] is synchronous (its reads are even infallible), while the
//! Qdrant client is async. The runtime therefore owns a small multi-threaded
//! Tokio runtime and bridges each call through [`block_on`](QdrantMemoryRuntime::block_on).
//! When invoked from *inside* an ambient Tokio runtime (e.g. the fused runtime's
//! turn, or the server boot under `#[tokio::main]`), it uses
//! [`tokio::task::block_in_place`] so it does not deadlock the caller's runtime;
//! otherwise it blocks on its own runtime directly.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ardur_embeddings::Embedder;
use ardur_memory::{
    HolderId, InvalidationReason, MemoryError, MemoryRecord, MemoryRuntime, RecordId, Result,
    UnixTsMillis,
};
use qdrant_client::Payload;
use qdrant_client::Qdrant;
use qdrant_client::qdrant::{
    Condition, CreateCollectionBuilder, CreateFieldIndexCollectionBuilder, Distance, FieldType,
    Filter, GetPointsBuilder, PointStruct, ScrollPointsBuilder, SearchPointsBuilder,
    UpsertPointsBuilder, Value, VectorParamsBuilder,
};
use uuid::Uuid;

use crate::config::QdrantMemoryConfig;
use crate::payload::{QdrantPayload, searchable_text};
use crate::snapshot::{MemorySnapshot, SnapshotReceiptSink};

/// The far end of an open-ended valid interval — mirrors the in-process store.
const FOREVER: UnixTsMillis = UnixTsMillis(u64::MAX);

/// An upper bound on points pulled per scroll. A single subject's correction
/// history is tiny in practice; `// TODO §7.0 Phase 2`: paginate the scroll
/// cursor for subjects with very long histories.
const SCROLL_LIMIT: u32 = 16_384;

/// A durable [`MemoryRuntime`] backed by a Qdrant collection.
pub struct QdrantMemoryRuntime {
    client: Qdrant,
    config: QdrantMemoryConfig,
    rt: tokio::runtime::Runtime,
    /// The model that turns a record's [`searchable_text`] into its stored
    /// vector. `None` keeps the legacy placeholder embedding (reads still work,
    /// since they scroll by payload filter; only vector *search* is meaningless).
    /// Attach a real model with [`with_embedder`](Self::with_embedder) for
    /// semantic recall.
    embedder: Option<Arc<dyn Embedder>>,
}

impl QdrantMemoryRuntime {
    /// Connect to Qdrant per `config` (no collection I/O yet — call
    /// [`init`](QdrantMemoryRuntime::init) to create the collection).
    ///
    /// # Errors
    /// [`MemoryError::Backend`] if the Tokio bridge runtime cannot be built or
    /// the Qdrant client cannot be constructed from the configured URL/key.
    pub fn connect(config: QdrantMemoryConfig) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| MemoryError::Backend(format!("building tokio runtime: {e}")))?;

        let mut builder = Qdrant::from_url(&config.url);
        if let Some(key) = &config.api_key {
            builder = builder.api_key(key.clone());
        }
        let client = builder
            .build()
            .map_err(|e| MemoryError::Backend(format!("building qdrant client: {e}")))?;

        Ok(Self {
            client,
            config,
            rt,
            embedder: None,
        })
    }

    /// Attach the embedding model used to vectorise each record's
    /// [`searchable_text`] on [`record`](MemoryRuntime::record).
    ///
    /// The collection's vector dimension is realigned to the embedder's output
    /// dimension, so call this **before** [`init`](Self::init) — otherwise the
    /// collection is created at the config dim and a later embed of a different
    /// dimension is rejected by Qdrant.
    #[must_use]
    pub fn with_embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.config.vector_dim = embedder.dimension();
        self.embedder = Some(embedder);
        self
    }

    /// Connect and then [`init`](QdrantMemoryRuntime::init) the collection — the
    /// convenience the server boot uses.
    ///
    /// # Errors
    /// Any error from [`connect`](QdrantMemoryRuntime::connect) or
    /// [`init`](QdrantMemoryRuntime::init).
    pub fn connect_and_init(config: QdrantMemoryConfig) -> Result<Self> {
        let this = Self::connect(config)?;
        this.init()?;
        Ok(this)
    }

    /// Create the collection (Cosine distance, the configured dim) if missing and
    /// reconcile payload indexes used by read/GC filters (`subject`,
    /// `channel_id`, `session_id`, `correction_chain_root`, and temporal fields)
    /// on every boot. Idempotent — safe to call on every boot, including after an
    /// older deployment created the collection without newer indexes.
    ///
    /// # Errors
    /// [`MemoryError::Backend`] on any Qdrant transport or collection error.
    pub fn init(&self) -> Result<()> {
        self.block_on(async {
            let exists = self
                .client
                .collection_exists(&self.config.collection_name)
                .await
                .map_err(|e| MemoryError::Backend(format!("collection_exists: {e}")))?;
            if !exists {
                self.client
                    .create_collection(
                        CreateCollectionBuilder::new(&self.config.collection_name).vectors_config(
                            VectorParamsBuilder::new(
                                self.config.vector_dim as u64,
                                Distance::Cosine,
                            ),
                        ),
                    )
                    .await
                    .map_err(|e| MemoryError::Backend(format!("create_collection: {e}")))?;
            }
            self.ensure_payload_indexes().await?;
            Ok(())
        })
    }

    async fn ensure_payload_indexes(&self) -> Result<()> {
        for (field, field_type) in payload_indexes() {
            let result = self
                .client
                .create_field_index(CreateFieldIndexCollectionBuilder::new(
                    &self.config.collection_name,
                    field,
                    field_type,
                ))
                .await;
            if let Err(e) = result {
                let msg = e.to_string();
                if !qdrant_index_already_exists(&msg) {
                    return Err(MemoryError::Backend(format!(
                        "create index on {field}: {e}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Take a Qdrant snapshot of the collection and record it as a
    /// [`MemorySnapshot`] event on `chain`, returning the snapshot id.
    ///
    /// # Errors
    /// [`MemoryError::Backend`] if the snapshot request fails or Qdrant returns
    /// no snapshot description.
    pub fn snapshot_into_receipt<S: SnapshotReceiptSink>(&self, chain: &mut S) -> Result<String> {
        let snapshot = self.create_snapshot()?;
        let id = snapshot.snapshot_id.clone();
        chain.append_memory_snapshot(snapshot);
        Ok(id)
    }

    /// Take a Qdrant snapshot of the collection, returning the [`MemorySnapshot`]
    /// descriptor (its id + the wall-clock instant it was taken).
    ///
    /// # Errors
    /// [`MemoryError::Backend`] if the snapshot request fails or Qdrant returns
    /// no snapshot name.
    pub fn create_snapshot(&self) -> Result<MemorySnapshot> {
        let name = self.block_on(async {
            let resp = self
                .client
                .create_snapshot(&self.config.collection_name)
                .await
                .map_err(|e| MemoryError::Backend(format!("create_snapshot: {e}")))?;
            resp.snapshot_description
                .and_then(|d| {
                    let name = d.name;
                    if name.is_empty() { None } else { Some(name) }
                })
                .ok_or_else(|| {
                    MemoryError::Backend("create_snapshot returned no snapshot name".to_string())
                })
        })?;
        Ok(MemorySnapshot {
            snapshot_id: name,
            ts: now_ms(),
        })
    }

    /// Drop the backing collection — test hygiene so a gated suite starts from a
    /// clean slate. Succeeds even if the collection does not exist.
    ///
    /// # Errors
    /// [`MemoryError::Backend`] on a Qdrant transport error.
    pub fn delete_collection(&self) -> Result<()> {
        self.block_on(async {
            self.client
                .delete_collection(&self.config.collection_name)
                .await
                .map(|_| ())
                .map_err(|e| MemoryError::Backend(format!("delete_collection: {e}")))
        })
    }

    /// The config this runtime was built with.
    #[must_use]
    pub fn config(&self) -> &QdrantMemoryConfig {
        &self.config
    }

    // ---- internals -------------------------------------------------------

    /// Build the Qdrant point for a record: id = the record UUID, vector = the
    /// real embedding of its [`searchable_text`] (or the placeholder when no
    /// embedder is attached), payload = the projected [`QdrantPayload`].
    fn point_for(&self, rec: &MemoryRecord) -> Result<PointStruct> {
        let payload = QdrantPayload::from_record(rec)?;
        let value = serde_json::to_value(&payload)
            .map_err(|e| MemoryError::Backend(format!("serialize payload: {e}")))?;
        let payload: Payload = Payload::try_from(value)
            .map_err(|e| MemoryError::Backend(format!("payload to qdrant: {e}")))?;
        let vector = self.embed_record(rec)?;
        Ok(PointStruct::new(rec.record_id.to_string(), vector, payload))
    }

    /// The stored vector for a record: the embedding of its
    /// [`searchable_text`](crate::searchable_text) when an [`Embedder`] is
    /// attached, else the legacy placeholder (a unit vector).
    fn embed_record(&self, rec: &MemoryRecord) -> Result<Vec<f32>> {
        match &self.embedder {
            Some(embedder) => self.embed_text(embedder, searchable_text(rec)),
            None => Ok(placeholder_embedding(self.config.vector_dim)),
        }
    }

    /// Embed a single text through `embedder`, bridging its async surface onto the
    /// runtime's blocking client.
    fn embed_text(&self, embedder: &Arc<dyn Embedder>, text: String) -> Result<Vec<f32>> {
        let mut out = self
            .block_on(embedder.embed(vec![text]))
            .map_err(|e| MemoryError::Backend(format!("embed: {e}")))?;
        out.pop()
            .ok_or_else(|| MemoryError::Backend("embedder returned no vector".to_string()))
    }

    /// Vector-search the collection with `query_vector`, returning up to `top_k`
    /// hits as `(record, similarity)` pairs ordered by similarity descending.
    ///
    /// This is the dense half of hybrid retrieval. Each hit carries the full
    /// reconstructed [`MemoryRecord`] (from the point's `record_json` payload), so
    /// no second fetch is needed to hydrate it. A point whose payload cannot be
    /// reconstructed is skipped.
    ///
    /// # Errors
    /// [`MemoryError::Backend`] on a Qdrant transport or search error.
    pub fn search_vectors(
        &self,
        query_vector: Vec<f32>,
        top_k: u64,
    ) -> Result<Vec<(MemoryRecord, f32)>> {
        if top_k == 0 {
            return Ok(Vec::new());
        }
        let resp = self.block_on(async {
            self.client
                .search_points(
                    SearchPointsBuilder::new(&self.config.collection_name, query_vector, top_k)
                        .with_payload(true),
                )
                .await
                .map_err(|e| MemoryError::Backend(format!("search_points: {e}")))
        })?;
        Ok(resp
            .result
            .into_iter()
            .filter_map(|p| record_from_payload(&p.payload).map(|r| (r, p.score)))
            .collect())
    }

    /// Embed `query` (requires an attached [`Embedder`]) and
    /// [`search_vectors`](Self::search_vectors) with the result.
    ///
    /// # Errors
    /// [`MemoryError::Backend`] if no embedder is attached, the embed fails, or the
    /// search fails.
    pub fn search_text(&self, query: &str, top_k: u64) -> Result<Vec<(MemoryRecord, f32)>> {
        let embedder = self.embedder.as_ref().ok_or_else(|| {
            MemoryError::Backend("search_text requires an attached embedder".to_string())
        })?;
        let vector = self.embed_text(embedder, query.to_string())?;
        self.search_vectors(vector, top_k)
    }

    /// Fetch a single record by its [`RecordId`] (the point id), if present — the
    /// public hydration hook a hybrid retriever uses to resolve a fused id.
    ///
    /// # Errors
    /// [`MemoryError::Backend`] on a Qdrant transport error.
    pub fn fetch_record(&self, id: RecordId) -> Result<Option<MemoryRecord>> {
        self.get_record(id.0)
    }

    /// Whether a real embedder is attached (vs. the placeholder vector).
    #[must_use]
    pub fn has_embedder(&self) -> bool {
        self.embedder.is_some()
    }

    /// Upsert one point, blocking on the bridge runtime.
    fn upsert(&self, point: PointStruct) -> Result<()> {
        self.block_on(async {
            self.client
                .upsert_points(
                    UpsertPointsBuilder::new(&self.config.collection_name, vec![point]).wait(true),
                )
                .await
                .map(|_| ())
                .map_err(|e| MemoryError::Backend(format!("upsert_points: {e}")))
        })
    }

    /// Scroll every point matching `filter` and reconstruct the records from the
    /// carried `record_json`. A point whose payload cannot be reconstructed is
    /// skipped (logged) rather than failing the whole read.
    fn scroll_records(&self, filter: Filter) -> Result<Vec<MemoryRecord>> {
        let points = self.block_on(async {
            self.client
                .scroll(
                    ScrollPointsBuilder::new(&self.config.collection_name)
                        .filter(filter)
                        .limit(SCROLL_LIMIT)
                        .with_payload(true)
                        .with_vectors(false),
                )
                .await
                .map_err(|e| MemoryError::Backend(format!("scroll: {e}")))
        })?;

        Ok(points
            .result
            .into_iter()
            .filter_map(|p| record_from_payload(&p.payload))
            .collect())
    }

    /// The set of `correction_chain_root`s tombstoned within `subject`'s records
    /// (or across **all** records when `subject` is `None`) — the chains hybrid
    /// recall must exclude so a forgotten memory is never re-injected (ARD-477).
    ///
    /// Scrolls the relevant records once and derives the dead set from their
    /// tombstones via the same chain-cutoff logic [`live_at`] applies. `pub(crate)`
    /// for the sibling [`HybridMemoryRetriever`](crate::HybridMemoryRetriever).
    ///
    /// # Errors
    /// [`MemoryError::Backend`] if the scroll fails.
    pub(crate) fn dead_chains(&self, subject: Option<&HolderId>) -> Result<HashSet<Uuid>> {
        let filter = match subject {
            Some(s) => Filter::must([Condition::matches("subject", s.0.clone())]),
            None => Filter::default(),
        };
        let records = self.scroll_records(filter)?;
        Ok(chain_cutoff_map(&records).into_keys().collect())
    }

    /// Fetch a single record by its UUID (the point id), if present.
    fn get_record(&self, id: Uuid) -> Result<Option<MemoryRecord>> {
        let points = self.block_on(async {
            self.client
                .get_points(
                    GetPointsBuilder::new(
                        &self.config.collection_name,
                        vec![id.to_string().into()],
                    )
                    .with_payload(true)
                    .with_vectors(false),
                )
                .await
                .map_err(|e| MemoryError::Backend(format!("get_points: {e}")))
        })?;
        Ok(points
            .result
            .into_iter()
            .find_map(|p| record_from_payload(&p.payload)))
    }

    /// Block on `fut`, cooperating with an ambient Tokio runtime when present.
    ///
    /// `pub(crate)` so the sibling [`HybridMemoryRetriever`](crate::HybridMemoryRetriever)
    /// can bridge its async recall onto this owned runtime when serving the
    /// synchronous `MemoryRuntime::search` (§7.0c).
    pub(crate) fn block_on<F>(&self, fut: F) -> F::Output
    where
        F: std::future::Future + Send,
        F::Output: Send,
    {
        block_on_runtime(&self.rt, fut)
    }
}

/// Drive `fut` to completion on the owned `rt`, cooperating with whatever
/// ambient Tokio runtime the caller is on.
///
/// The subtlety is `block_in_place`: it is only legal under a **multi-thread**
/// runtime and **panics** under a current-thread one (M0c). The previous code
/// always used it when an ambient runtime was present, so a caller on
/// `#[tokio::main(flavor = "current_thread")]` turned every sync memory op into
/// a panic. Dispatch on the flavor: block-in-place under multi-thread, and under
/// a current-thread ambient runtime fall back to a dedicated scoped thread
/// (calling `self.rt.block_on` on the ambient thread would itself panic —
/// "cannot start a runtime from within a runtime").
fn block_on_runtime<F>(rt: &tokio::runtime::Runtime, fut: F) -> F::Output
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    use tokio::runtime::RuntimeFlavor;

    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| rt.block_on(fut))
        }
        // Current-thread (or any non-multi-thread) ambient runtime: run on a
        // dedicated thread that owns no ambient runtime, so `rt.block_on` is
        // legal there. `scope` lets the thread borrow `rt` and `fut` without a
        // `'static` bound.
        Ok(_) => std::thread::scope(|scope| {
            scope
                .spawn(|| rt.block_on(fut))
                .join()
                .expect("memory block_on worker thread panicked")
        }),
        // No ambient runtime: drive directly.
        Err(_) => rt.block_on(fut),
    }
}

fn payload_indexes() -> Vec<(&'static str, FieldType)> {
    vec![
        ("subject", FieldType::Keyword),
        ("channel_id", FieldType::Keyword),
        ("session_id", FieldType::Keyword),
        ("correction_chain_root", FieldType::Keyword),
        ("event_time", FieldType::Integer),
        ("valid_from", FieldType::Integer),
        ("valid_to", FieldType::Integer),
        ("invalidation_time", FieldType::Integer),
    ]
}

fn qdrant_index_already_exists(message: &str) -> bool {
    let msg = message.to_ascii_lowercase();
    msg.contains("already exists")
        || msg.contains("already has")
        || msg.contains("already been created")
}

impl MemoryRuntime for QdrantMemoryRuntime {
    fn record(&self, rec: MemoryRecord) -> Result<RecordId> {
        let id = rec.record_id;
        let point = self.point_for(&rec)?;
        self.upsert(point)?;
        Ok(RecordId(id))
    }

    fn at_time(&self, subject: &HolderId, as_of: UnixTsMillis) -> Vec<MemoryRecord> {
        let filter = Filter::must([Condition::matches("subject", subject.0.clone())]);
        match self.scroll_records(filter) {
            Ok(records) => live_at(&records, as_of),
            Err(e) => {
                tracing::warn!(error = %e, subject = %subject.0, "qdrant at_time read failed");
                Vec::new()
            }
        }
    }

    fn history_of(&self, record_id: RecordId) -> Vec<MemoryRecord> {
        let root = match self.get_record(record_id.0) {
            Ok(Some(rec)) => rec.correction_chain_root,
            Ok(None) => return Vec::new(),
            Err(e) => {
                tracing::warn!(error = %e, "qdrant history_of root lookup failed");
                return Vec::new();
            }
        };
        let filter = Filter::must([Condition::matches(
            "correction_chain_root",
            root.to_string(),
        )]);
        match self.scroll_records(filter) {
            Ok(mut records) => {
                // Scroll order is unspecified; approximate insertion order with
                // the transaction-time axis so a chain reads oldest-first.
                records.sort_by_key(|r| r.recorded_at);
                records
            }
            Err(e) => {
                tracing::warn!(error = %e, "qdrant history_of scroll failed");
                Vec::new()
            }
        }
    }

    fn invalidate(
        &self,
        record_id: RecordId,
        at: UnixTsMillis,
        reason: InvalidationReason,
    ) -> Result<()> {
        let target = self
            .get_record(record_id.0)?
            .ok_or(MemoryError::NotFound(record_id.0))?;
        let tombstone = tombstone_for(&target, at, reason);
        // Reuse the upsert path; the tombstone is just another point.
        self.record(tombstone).map(|_| ())
    }
}

/// The fallback embedding used when no [`Embedder`] is attached: a unit vector
/// (`[1, 0, 0, …]`). The bi-temporal `at_time`/`history_of` reads scroll by
/// payload filter, so the vector's content does not affect *their* correctness;
/// a unit vector (rather than all-zeros) keeps the point valid under Cosine
/// distance. Attach a model with [`QdrantMemoryRuntime::with_embedder`] for the
/// real semantic vector.
fn placeholder_embedding(dim: usize) -> Vec<f32> {
    let mut v = vec![0.0_f32; dim];
    if let Some(first) = v.first_mut() {
        *first = 1.0;
    }
    v
}

/// Reconstruct a record from a retrieved point's payload (`record_json`).
fn record_from_payload(payload: &HashMap<String, Value>) -> Option<MemoryRecord> {
    let raw = payload.get("record_json")?.as_str()?;
    match serde_json::from_str(raw) {
        Ok(rec) => Some(rec),
        Err(e) => {
            tracing::warn!(error = %e, "skipping point with unreadable record_json");
            None
        }
    }
}

/// Per-`correction_chain_root` earliest invalidation cutoff over a set of
/// records (including tombstones). A chain present in the map has been
/// invalidated. Shared by [`live_at`] (bi-temporal view, compared against
/// `as_of`) and [`QdrantMemoryRuntime::dead_chains`] (live recall view: any
/// chain with a tombstone is dead).
fn chain_cutoff_map(records: &[MemoryRecord]) -> HashMap<Uuid, UnixTsMillis> {
    let mut cutoff: HashMap<Uuid, UnixTsMillis> = HashMap::new();
    for r in records {
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
    cutoff
}

/// The bi-temporal "as-of" view over a set of a single subject's records — the
/// same predicate the in-process [`ardur_memory::InMemoryMemoryRuntime`] applies:
/// live data rows within their valid interval whose correction chain has not been
/// cut off at or before `as_of`.
fn live_at(records: &[MemoryRecord], as_of: UnixTsMillis) -> Vec<MemoryRecord> {
    let cutoff = chain_cutoff_map(records);

    // Live data rows still within their valid interval and not yet cut off by
    // their chain's invalidation.
    records
        .iter()
        .filter(|r| r.invalidation_time.is_none())
        .filter(|r| r.valid_from <= as_of && as_of < r.valid_to.unwrap_or(FOREVER))
        .filter(|r| match cutoff.get(&r.correction_chain_root) {
            Some(cut) => *cut > as_of,
            None => true,
        })
        .cloned()
        .collect()
}

/// Build the invalidation tombstone for `target` — a new row inheriting the
/// target's correction chain with `invalidation_time = at`. Mirrors the
/// in-process store's `invalidate`.
fn tombstone_for(
    target: &MemoryRecord,
    at: UnixTsMillis,
    reason: InvalidationReason,
) -> MemoryRecord {
    MemoryRecord {
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
    }
}

/// The current wall clock in milliseconds since the Unix epoch (for snapshot
/// timestamps). Falls back to `0` if the system clock is before the epoch.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ardur_memory::{HolderId, RecordKind};

    fn owned_multi_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("owned runtime builds")
    }

    /// M0c: a sync memory op under a **current-thread** ambient runtime must not
    /// panic. The old `block_in_place` path panicked here; the flavor-aware
    /// fallback drives the future on a dedicated thread instead.
    #[test]
    fn block_on_runtime_survives_a_current_thread_ambient_runtime() {
        let owned = owned_multi_thread_runtime();
        let ambient = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread ambient runtime builds");
        let out = ambient.block_on(async { block_on_runtime(&owned, async { 21 * 2 }) });
        assert_eq!(out, 42);
    }

    /// The multi-thread ambient path (production) still drives the future.
    #[test]
    fn block_on_runtime_works_under_a_multi_thread_ambient_runtime() {
        let owned = owned_multi_thread_runtime();
        let ambient = owned_multi_thread_runtime();
        let out = ambient.block_on(async { block_on_runtime(&owned, async { 7 + 35 }) });
        assert_eq!(out, 42);
    }

    /// And with no ambient runtime at all it drives directly.
    #[test]
    fn block_on_runtime_works_with_no_ambient_runtime() {
        let owned = owned_multi_thread_runtime();
        assert_eq!(block_on_runtime(&owned, async { 40 + 2 }), 42);
    }

    fn fact(
        subject: &str,
        payload: serde_json::Value,
        t: u64,
        valid_to: Option<u64>,
    ) -> MemoryRecord {
        MemoryRecord::new(
            HolderId::from(subject),
            RecordKind::Preference,
            payload,
            UnixTsMillis(t),
            UnixTsMillis(t),
            valid_to.map(UnixTsMillis),
            UnixTsMillis(t),
        )
    }

    #[test]
    fn placeholder_embedding_is_a_unit_vector_of_the_right_dim() {
        let v = placeholder_embedding(4);
        assert_eq!(v, vec![1.0, 0.0, 0.0, 0.0]);
        assert!(placeholder_embedding(0).is_empty());
    }

    #[test]
    fn live_at_honors_valid_interval_and_chain_cutoff() {
        let user = "user:live-at";
        let f1 = fact(user, serde_json::json!("tea"), 1_000, None);
        let f2 = fact(user, serde_json::json!("coffee"), 2_000, None);
        let tomb = tombstone_for(&f1, UnixTsMillis(2_000), InvalidationReason::Superseded);
        let all = vec![f1.clone(), f2.clone(), tomb];

        // Before f1 is valid: nothing.
        assert!(live_at(&all, UnixTsMillis(999)).is_empty());
        // Between f1 and the cutoff: tea.
        let mid = live_at(&all, UnixTsMillis(1_500));
        assert_eq!(mid.len(), 1);
        assert_eq!(mid[0].payload, serde_json::json!("tea"));
        // After the cutoff: coffee only (f1's chain is cut at 2_000, exclusive).
        let now = live_at(&all, UnixTsMillis(3_000));
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].payload, serde_json::json!("coffee"));
    }

    #[test]
    fn tombstone_inherits_chain_and_carries_cutoff() {
        let f1 = fact("user:tomb", serde_json::json!("v1"), 1_000, None);
        let tomb = tombstone_for(&f1, UnixTsMillis(2_000), InvalidationReason::UserCorrection);
        assert_eq!(tomb.correction_chain_root, f1.correction_chain_root);
        assert_eq!(tomb.invalidation_time, Some(UnixTsMillis(2_000)));
        assert_ne!(tomb.record_id, f1.record_id);
    }

    #[test]
    fn payload_indexes_cover_every_filter_field() {
        let indexes: HashMap<_, _> = payload_indexes().into_iter().collect();
        assert_eq!(indexes.get("subject"), Some(&FieldType::Keyword));
        assert_eq!(indexes.get("channel_id"), Some(&FieldType::Keyword));
        assert_eq!(indexes.get("session_id"), Some(&FieldType::Keyword));
        assert_eq!(
            indexes.get("correction_chain_root"),
            Some(&FieldType::Keyword)
        );
        assert_eq!(indexes.get("event_time"), Some(&FieldType::Integer));
        assert_eq!(indexes.get("valid_from"), Some(&FieldType::Integer));
        assert_eq!(indexes.get("valid_to"), Some(&FieldType::Integer));
        assert_eq!(indexes.get("invalidation_time"), Some(&FieldType::Integer));
    }

    #[test]
    fn existing_index_errors_are_idempotent() {
        assert!(qdrant_index_already_exists("Index already exists"));
        assert!(qdrant_index_already_exists(
            "Bad request: collection already has an index for subject"
        ));
        assert!(!qdrant_index_already_exists("transport unavailable"));
    }
}
