//! [`HybridMemoryRetriever`] — dense + sparse recall over the durable store.
//!
//! Hybrid retrieval runs a query through two complementary retrievers and fuses
//! their ranked results:
//!
//! - **dense** — the query is embedded ([`ardur_embeddings`]) and matched against
//!   each record's stored vector via Qdrant ANN search. Matches on *meaning*: a
//!   query and a record close in embedding space score highly even with no shared
//!   words.
//! - **sparse** — a BM25 lexical index ([`ardur_bm25_index`]) matches on *terms*,
//!   rewarding records that contain the query's exact words.
//!
//! The two score on incomparable scales (cosine in `[-1, 1]`, BM25 in `[0, ∞)`),
//! so the lists are combined with rank-based reciprocal-rank fusion
//! ([`ardur_fusion::reciprocal_rank_fusion`]) — a record several retrievers rank
//! highly floats to the top, with no cross-scale score comparison.
//!
//! Both halves are written on [`record`](HybridMemoryRetriever::record): the
//! durable [`QdrantMemoryRuntime`] gets the embedded point (its `record_json`
//! payload is the lossless source of truth for hydration), and the BM25 index
//! gets the same [`searchable_text`](crate::searchable_text). The retriever shares
//! one [`Embedder`] with the underlying runtime so a record and a query are always
//! embedded by the same model.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ardur_bm25_index::Bm25Index;
use ardur_embeddings::Embedder;
use ardur_fusion::{DEFAULT_RRF_K, ScoredDoc, reciprocal_rank_fusion};
use ardur_memory::{
    HolderId, InvalidationReason, MemoryError, MemoryRecord, MemoryRuntime, RecordId, Result,
    UnixTsMillis,
};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::QdrantMemoryRuntime;
use crate::payload::searchable_text;

/// How many candidates to pull from *each* retriever before fusion, as a multiple
/// of the requested `top_k`. Over-fetching gives reciprocal-rank fusion enough
/// overlap to reward records both retrievers surface, and headroom to skip any
/// invalidation tombstones that slip into the candidate pool.
const CANDIDATE_MULTIPLIER: usize = 4;

/// A floor on the per-retriever candidate pool, so a tiny `top_k` (e.g. 1) still
/// fetches a meaningful spread to fuse over.
const MIN_CANDIDATES: usize = 32;

/// Hybrid (dense + sparse) recall over the durable Qdrant store.
///
/// Owns the durable [`QdrantMemoryRuntime`] (the dense half + the hydration
/// source) and a [`Bm25Index`] (the sparse half), and shares one [`Embedder`]
/// with the runtime. Construct with [`new`](Self::new); write with
/// [`record`](Self::record); recall with [`search`](Self::search).
pub struct HybridMemoryRetriever {
    /// The durable store: holds the embedded points and serves both vector search
    /// and id-hydration. The shared embedder is attached here, so its `record`
    /// embeds with the same model the retriever queries with.
    qdrant: QdrantMemoryRuntime,
    /// The sparse lexical index. `Bm25Index::add` takes `&mut self`, so it sits
    /// behind a `Mutex` held across the (async) commit.
    bm25: Mutex<Bm25Index>,
    /// The shared embedding model — embeds queries here, and records via the
    /// `qdrant` runtime it was also attached to.
    embedder: Arc<dyn Embedder>,
}

impl HybridMemoryRetriever {
    /// Assemble a retriever from a connected (but not necessarily initialised)
    /// durable runtime, a BM25 index, and an embedder.
    ///
    /// The embedder is attached to `qdrant` (realigning the collection's vector
    /// dim to the model's output dim), so initialise the collection — via
    /// [`QdrantMemoryRuntime::init`] on [`qdrant`](Self::qdrant) — **after**
    /// constructing the retriever, never before.
    pub fn new(qdrant: QdrantMemoryRuntime, bm25: Bm25Index, embedder: Arc<dyn Embedder>) -> Self {
        let qdrant = qdrant.with_embedder(Arc::clone(&embedder));
        Self {
            qdrant,
            bm25: Mutex::new(bm25),
            embedder,
        }
    }

    /// The durable runtime — for collection init/teardown, bi-temporal `at_time`
    /// reads, and snapshots. (Borrow-only; the retriever owns it.)
    #[must_use]
    pub fn qdrant(&self) -> &QdrantMemoryRuntime {
        &self.qdrant
    }

    /// Write `rec` to **both** backends: the durable Qdrant store (embedded point)
    /// and the BM25 lexical index (same [`searchable_text`](crate::searchable_text)).
    ///
    /// The durable store is written first (it is the source of truth); a BM25
    /// failure after a successful Qdrant write surfaces as an error, leaving the
    /// record durably stored but absent from the lexical half until re-indexed.
    ///
    /// # Errors
    /// [`MemoryError::Backend`] if either write fails.
    pub async fn record(&self, rec: MemoryRecord) -> Result<RecordId> {
        let doc_id = rec.record_id.to_string();
        let text = searchable_text(&rec);
        let record_id = self.qdrant.record_async(rec).await?;
        self.bm25
            .lock()
            .await
            .add(doc_id, text)
            .await
            .map_err(|e| MemoryError::Backend(format!("bm25 add: {e}")))?;
        Ok(record_id)
    }

    /// Recall the `top_k` records most relevant to `query`, fusing dense vector
    /// search with sparse BM25 lexical search.
    ///
    /// Invalidation tombstones are never returned (they carry an
    /// `invalidation_time`); the fused candidate pool is over-fetched so a
    /// tombstone in it does not cost a live result.
    ///
    /// # Errors
    /// [`MemoryError::Backend`] if the query embed, the vector search, or the BM25
    /// query fails.
    pub async fn search(&self, query: &str, top_k: usize) -> Result<Vec<MemoryRecord>> {
        self.search_filtered(query, top_k, None).await
    }

    /// Recall memories relevant to `query`, restricted to one holder/workspace.
    ///
    /// The filter is applied before hydration returns records to callers, so a
    /// memory hit from another workspace can never cross the API boundary. The
    /// dense and sparse candidate pools are still over-fetched before filtering;
    /// a future Qdrant-side subject filter can make this cheaper without
    /// changing the isolation semantics.
    pub async fn search_for_subject(
        &self,
        subject: &HolderId,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<MemoryRecord>> {
        self.search_filtered(query, top_k, Some(subject)).await
    }

    async fn search_filtered(
        &self,
        query: &str,
        top_k: usize,
        subject: Option<&HolderId>,
    ) -> Result<Vec<MemoryRecord>> {
        if query.trim().is_empty() || top_k == 0 {
            return Ok(Vec::new());
        }
        let candidate_k = candidate_pool(top_k);

        // ARD-477: exclude any chain that has been tombstoned so a forgotten
        // memory is never re-injected. One scroll of the relevant records.
        // Awaited directly (not the sync bridge) so this recall runs on a single
        // `block_on` pass — a nested bridge here re-enters the owned runtime and
        // panics the turn (#348).
        let dead = self.qdrant.dead_chains(subject).await?;

        // ---- dense: embed the query, ANN-search, drop tombstones, and keep the
        //      hydrated records (vector hits carry their full record_json).
        let query_vec = self.embed_query(query).await?;
        let vector_hits = self
            .qdrant
            .search_vectors_async(query_vec, candidate_k as u64)
            .await?;
        let mut hydrated: HashMap<String, MemoryRecord> = HashMap::new();
        let mut vector_list: Vec<ScoredDoc> = Vec::with_capacity(vector_hits.len());
        for (rec, score) in vector_hits {
            if rec.invalidation_time.is_some() {
                continue;
            }
            if dead.contains(&rec.correction_chain_root) {
                continue;
            }
            if subject.is_some_and(|s| &rec.subject != s) {
                continue;
            }
            let id = rec.record_id.to_string();
            vector_list.push(ScoredDoc::new(id.clone(), f64::from(score)));
            hydrated.insert(id, rec);
        }

        // ---- sparse: BM25 over the same query.
        let lexical_list: Vec<ScoredDoc> = self
            .bm25
            .lock()
            .await
            .query(query, candidate_k)
            .await
            .map_err(|e| MemoryError::Backend(format!("bm25 query: {e}")))?
            .into_iter()
            .map(|d| ScoredDoc::new(d.doc_id, f64::from(d.score)))
            .collect();

        // ---- fuse on rank, then hydrate in fused order, skipping tombstones,
        //      until `top_k` live records are collected.
        let fused = fuse(vector_list, lexical_list, candidate_k);
        let mut out = Vec::with_capacity(top_k);
        for doc in fused {
            if out.len() >= top_k {
                break;
            }
            let rec = match hydrated.remove(&doc.doc_id) {
                Some(rec) => rec,
                None => match self.fetch_live(&doc.doc_id, subject, &dead).await? {
                    Some(rec) => rec,
                    None => continue,
                },
            };
            out.push(rec);
        }
        Ok(out)
    }

    /// Embed a single query string through the shared model.
    async fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let mut out = self
            .embedder
            .embed(vec![query.to_string()])
            .await
            .map_err(|e| MemoryError::Backend(format!("embed query: {e}")))?;
        out.pop()
            .ok_or_else(|| MemoryError::Backend("embedder returned no vector".to_string()))
    }

    /// Hydrate a fused `doc_id` from the durable store, returning it only if it is
    /// a live (non-tombstone) record. An unparseable id or a missing point yields
    /// `None`.
    async fn fetch_live(
        &self,
        doc_id: &str,
        subject: Option<&HolderId>,
        dead: &HashSet<Uuid>,
    ) -> Result<Option<MemoryRecord>> {
        let Ok(uuid) = Uuid::parse_str(doc_id) else {
            return Ok(None);
        };
        Ok(self
            .qdrant
            .fetch_record_async(RecordId(uuid))
            .await?
            .filter(|rec| rec.invalidation_time.is_none())
            .filter(|rec| !dead.contains(&rec.correction_chain_root))
            .filter(|rec| subject.is_none_or(|s| &rec.subject == s)))
    }
}

/// The retriever *is* a [`MemoryRuntime`] (§7.0c): this is what lets boot wrap it
/// behind the same `Arc<dyn MemoryRuntime>` seam as the in-process and bare
/// durable stores, while uniquely overriding [`search`](MemoryRuntime::search)
/// with real dense+sparse fusion.
///
/// The bi-temporal methods (`at_time`, `history_of`, `invalidate`) delegate
/// straight to the durable [`QdrantMemoryRuntime`]. The write/recall methods —
/// [`record`](MemoryRuntime::record) and [`search`](MemoryRuntime::search) — are
/// asynchronous on the inherent API (dual-write to Qdrant **and** the BM25 index;
/// fused recall over both), so each synchronous trait method bridges onto the
/// runtime's own Tokio executor with a **single** outer `block_on` over a fully
/// async body. That body `await`s the durable store's `*_async` cores directly
/// (`record_async`, `search_vectors_async`, `dead_chains`, `fetch_record_async`)
/// rather than the sync methods, so it never re-enters the owned runtime — the
/// nested-bridge recall panic (#348). `self.record(..)` and `self.search(..)`
/// below resolve to the *inherent* async methods (inherent methods shadow trait
/// methods of the same name), so there is no recursion.
impl MemoryRuntime for HybridMemoryRetriever {
    fn record(&self, rec: MemoryRecord) -> Result<RecordId> {
        self.qdrant.block_on(self.record(rec))
    }

    fn at_time(&self, subject: &HolderId, as_of: UnixTsMillis) -> Vec<MemoryRecord> {
        self.qdrant.at_time(subject, as_of)
    }

    fn history_of(&self, record_id: RecordId) -> Vec<MemoryRecord> {
        self.qdrant.history_of(record_id)
    }

    fn invalidate(
        &self,
        record_id: RecordId,
        at: UnixTsMillis,
        reason: InvalidationReason,
    ) -> Result<()> {
        // ARD-504: Qdrant is the source of truth and records the tombstone first.
        // After that succeeds, GC every already-indexed row in the invalidated
        // correction chain from BM25 so lexical recall does not accumulate stale
        // candidates forever. If BM25 cleanup fails, surface the error; Qdrant's
        // tombstone still prevents stale hydration from crossing the API boundary.
        let chain = self.qdrant.history_of(record_id);
        self.qdrant.invalidate(record_id, at, reason)?;
        self.qdrant.block_on(async {
            let mut bm25 = self.bm25.lock().await;
            for rec in chain {
                bm25.delete(&rec.record_id.to_string())
                    .await
                    .map_err(|e| MemoryError::Backend(format!("bm25 delete: {e}")))?;
            }
            Ok(())
        })
    }

    fn search(&self, query: &str, top_k: usize) -> Result<Vec<MemoryRecord>> {
        self.qdrant.block_on(self.search(query, top_k))
    }

    fn search_scoped(
        &self,
        subject: &HolderId,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<MemoryRecord>> {
        self.qdrant
            .block_on(self.search_for_subject(subject, query, top_k))
    }
}

/// The per-retriever candidate pool size for a requested `top_k`.
fn candidate_pool(top_k: usize) -> usize {
    top_k
        .saturating_mul(CANDIDATE_MULTIPLIER)
        .max(MIN_CANDIDATES)
}

/// Fuse the dense (vector) and sparse (BM25) candidate lists into one ranked list
/// of record ids via reciprocal-rank fusion — the rank-based strategy that, by
/// never comparing the retrievers' incomparable raw scores, is robust to their
/// wildly different scales.
fn fuse(vector: Vec<ScoredDoc>, lexical: Vec<ScoredDoc>, top_k: usize) -> Vec<ScoredDoc> {
    reciprocal_rank_fusion(vec![vector, lexical], DEFAULT_RRF_K, top_k)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(docs: &[ScoredDoc]) -> Vec<&str> {
        docs.iter().map(|d| d.doc_id.as_str()).collect()
    }

    #[test]
    fn candidate_pool_over_fetches_with_a_floor() {
        assert_eq!(candidate_pool(1), MIN_CANDIDATES);
        assert_eq!(candidate_pool(10), 40);
        assert_eq!(candidate_pool(100), 400);
    }

    /// A record with a strong *lexical* hit (BM25 rank-0) outranks one that is
    /// only weakly *semantically* near (dense, lower rank) — the lexical signal
    /// surfaces it even though the dense retriever did not put it first.
    #[test]
    fn lexical_hit_first() {
        // Dense: "a" is the nearest, "b" only third. Sparse: "b" is the exact
        // term hit. Fused: b = 1/60 (lex r0) + 1/62 (vec r2); a = 1/60 (vec r0).
        let vector = vec![
            ScoredDoc::new("a", 0.91),
            ScoredDoc::new("c", 0.70),
            ScoredDoc::new("b", 0.40),
        ];
        let lexical = vec![ScoredDoc::new("b", 12.0)];
        let out = fuse(vector, lexical, 10);
        assert_eq!(out[0].doc_id, "b", "the strong lexical hit leads");
        let b = 1.0 / 60.0 + 1.0 / 62.0;
        assert!((out[0].score - b).abs() < 1e-12);
    }

    /// A record both retrievers rank *moderately* beats records either retriever
    /// alone ranks #1 — the core reason to fuse rather than pick one retriever.
    #[test]
    fn hybrid_beats_either() {
        // "shared" is rank-1 in both lists; "x"/"y" are each rank-0 in only one.
        let vector = vec![ScoredDoc::new("x", 0.95), ScoredDoc::new("shared", 0.55)];
        let lexical = vec![ScoredDoc::new("y", 18.0), ScoredDoc::new("shared", 4.0)];
        let out = fuse(vector, lexical, 10);
        assert_eq!(out[0].doc_id, "shared");
        let shared = 1.0 / 61.0 + 1.0 / 61.0;
        assert!((out[0].score - shared).abs() < 1e-12);
        // It outscores the single-list leaders x and y (each 1/60).
        assert!(out[0].score > 1.0 / 60.0);
    }

    /// Fusion truncates to `top_k`.
    #[test]
    fn top_k() {
        let vector = vec![
            ScoredDoc::new("a", 0.9),
            ScoredDoc::new("b", 0.8),
            ScoredDoc::new("c", 0.7),
        ];
        let lexical = vec![ScoredDoc::new("d", 5.0), ScoredDoc::new("e", 1.0)];
        let out = fuse(vector, lexical, 2);
        assert_eq!(out.len(), 2);
        // Rank-0 docs (a, d) tie above the rest; tie-break is doc_id ascending.
        assert_eq!(ids(&out), vec!["a", "d"]);
    }
}
