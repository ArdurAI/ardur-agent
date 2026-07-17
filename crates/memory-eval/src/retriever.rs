//! The retrievers the harness measures: a lexical BM25 retriever, a dense
//! embedding retriever, and the [`HybridRetriever`] that fuses them with
//! Reciprocal Rank Fusion — the baseline any future graph-RAG route must beat.
//!
//! All three implement one [`Retriever`] trait returning ranked [`ScoredDoc`]s,
//! so the runner scores them through identical code and their numbers are
//! directly comparable.

use std::sync::Arc;

use async_trait::async_trait;

use ardur_bm25_index::Bm25Index;
use ardur_embeddings::Embedder;
pub use ardur_fusion::ScoredDoc;
use ardur_fusion::{DEFAULT_RRF_K, reciprocal_rank_fusion};

use crate::corpus::EvalDoc;

/// The default candidate pool depth each sub-retriever contributes to fusion
/// before the fused list is truncated to the caller's `top_k`. Deep enough that
/// a doc one retriever ranks poorly can still be rescued by the other (V3's
/// "top-50 fused" convention).
pub const DEFAULT_CANDIDATE_POOL: usize = 50;

/// Errors a retriever can surface.
#[derive(Debug, thiserror::Error)]
pub enum RetrieveError {
    /// The BM25 index failed to build or query.
    #[error("bm25: {0}")]
    Bm25(#[from] ardur_bm25_index::Bm25Error),
    /// The dense embedder failed.
    #[error("embed: {0}")]
    Embed(#[from] ardur_embeddings::EmbedError),
}

/// Ranked `doc_id`s (best first) from a scored result list — the shape the
/// metrics consume.
pub fn ranked_ids(hits: &[ScoredDoc]) -> Vec<String> {
    hits.iter().map(|h| h.doc_id.clone()).collect()
}

/// A retriever: given a query, return up to `top_k` scored docs, best first.
#[async_trait]
pub trait Retriever: Send + Sync {
    /// A stable name for reporting (e.g. `"bm25"`, `"dense"`, `"hybrid-rrf"`).
    fn name(&self) -> &str;

    /// Retrieve up to `top_k` docs for `query`, ranked best-first.
    async fn retrieve(&self, query: &str, top_k: usize) -> Result<Vec<ScoredDoc>, RetrieveError>;
}

/// A lexical retriever over a Tantivy BM25 index — the half of hybrid that nails
/// exact strings: filenames, identifiers, commands, error messages.
pub struct Bm25Retriever {
    index: Bm25Index,
}

impl Bm25Retriever {
    /// Build an in-RAM BM25 index over `docs` (hermetic — no directory, vanishes
    /// on drop).
    ///
    /// # Errors
    ///
    /// Returns a [`RetrieveError::Bm25`] if the index cannot be built.
    pub async fn index(docs: &[EvalDoc]) -> Result<Self, RetrieveError> {
        let mut index = Bm25Index::new(None)?;
        for doc in docs {
            index.add(doc.id.clone(), doc.text.clone()).await?;
        }
        Ok(Self { index })
    }
}

#[async_trait]
impl Retriever for Bm25Retriever {
    fn name(&self) -> &str {
        "bm25"
    }

    async fn retrieve(&self, query: &str, top_k: usize) -> Result<Vec<ScoredDoc>, RetrieveError> {
        // Sanitize the query into bare terms: Tantivy's query parser treats
        // characters like `?`, `:`, `/` as syntax, so a natural-language golden
        // query ("what port does the server bind?") would fail to parse. The
        // eval harness wants bag-of-words BM25, so we keep alphanumerics and
        // whitespace and drop the rest.
        let sanitized = sanitize_query(query);
        if sanitized.is_empty() {
            return Ok(Vec::new());
        }
        let hits = self.index.query(&sanitized, top_k).await?;
        Ok(hits
            .into_iter()
            .map(|h| ScoredDoc::new(h.doc_id, f64::from(h.score)))
            .collect())
    }
}

/// Lowercase `query` and keep only alphanumeric characters and spaces, so the
/// BM25 query parser sees bag-of-words terms rather than query syntax.
fn sanitize_query(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for c in query.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else {
            // Whitespace and punctuation alike become a separator, so the BM25
            // parser sees bare terms.
            out.push(' ');
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A dense retriever: brute-force cosine similarity over embedded documents.
///
/// Generic over any [`Embedder`], so the same code path measures the
/// deterministic `MockEmbedder` (hermetic CI) and the real `FastEmbedEmbedder`
/// (the `live-embed` baseline). Brute-force is intentional — the golden corpora
/// are small, and an exact scan removes ANN recall as a confounder so the
/// baseline reflects the *embedding's* quality, not the index's.
pub struct DenseRetriever<E: Embedder> {
    embedder: E,
    /// `(doc_id, unit-or-raw embedding)`.
    docs: Vec<(String, Vec<f32>)>,
}

impl<E: Embedder> DenseRetriever<E> {
    /// Embed every doc's text up front.
    ///
    /// # Errors
    ///
    /// Returns [`RetrieveError::Embed`] if embedding fails.
    pub async fn index(embedder: E, docs: &[EvalDoc]) -> Result<Self, RetrieveError> {
        let texts: Vec<String> = docs.iter().map(|d| d.text.clone()).collect();
        let vectors = embedder.embed(texts).await?;
        let docs = docs
            .iter()
            .zip(vectors)
            .map(|(d, v)| (d.id.clone(), v))
            .collect();
        Ok(Self { embedder, docs })
    }
}

/// Cosine similarity of two equal-length vectors; `0.0` if either is degenerate.
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (&x, &y) in a.iter().zip(b) {
        dot += f64::from(x) * f64::from(y);
        na += f64::from(x) * f64::from(x);
        nb += f64::from(y) * f64::from(y);
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

#[async_trait]
impl<E: Embedder> Retriever for DenseRetriever<E> {
    fn name(&self) -> &str {
        "dense"
    }

    async fn retrieve(&self, query: &str, top_k: usize) -> Result<Vec<ScoredDoc>, RetrieveError> {
        if top_k == 0 || self.docs.is_empty() {
            return Ok(Vec::new());
        }
        let mut q = self.embedder.embed(vec![query.to_string()]).await?;
        let qv = q.pop().unwrap_or_default();
        let mut scored: Vec<ScoredDoc> = self
            .docs
            .iter()
            .map(|(id, v)| ScoredDoc::new(id.clone(), cosine(&qv, v)))
            .collect();
        // Descending by score; deterministic doc_id tie-break so runs are stable.
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.doc_id.cmp(&b.doc_id))
        });
        scored.truncate(top_k);
        Ok(scored)
    }
}

/// The hybrid baseline: fuse a dense and a lexical retriever with Reciprocal Rank
/// Fusion (RRF, `k = 60`, Cormack et al. SIGIR 2009).
///
/// RRF ranks each sub-retriever's pool and sums `1 / (rank + k)` per doc, so it
/// combines the two signals without needing their scores to be on a comparable
/// scale — dense cosine and BM25 magnitudes never have to be reconciled. This is
/// the number graph-RAG must beat (V3 Q5-a: `recall@5 >= 1.10x` this baseline).
pub struct HybridRetriever {
    dense: Arc<dyn Retriever>,
    lexical: Arc<dyn Retriever>,
    rrf_k: f64,
    candidate_pool: usize,
    name: String,
}

impl HybridRetriever {
    /// Fuse `dense` and `lexical` with the default RRF `k = 60` and candidate
    /// pool depth. Sub-retrievers are shared (`Arc`) so they can also be scored
    /// standalone in the same report without re-indexing.
    pub fn new(dense: Arc<dyn Retriever>, lexical: Arc<dyn Retriever>) -> Self {
        Self {
            dense,
            lexical,
            rrf_k: DEFAULT_RRF_K,
            candidate_pool: DEFAULT_CANDIDATE_POOL,
            name: "hybrid-rrf".to_string(),
        }
    }

    /// Override the RRF `k` constant (default [`DEFAULT_RRF_K`] = 60).
    #[must_use]
    pub fn with_rrf_k(mut self, k: f64) -> Self {
        self.rrf_k = k;
        self
    }

    /// Override the per-retriever candidate pool depth (default
    /// [`DEFAULT_CANDIDATE_POOL`]).
    #[must_use]
    pub fn with_candidate_pool(mut self, pool: usize) -> Self {
        self.candidate_pool = pool;
        self
    }
}

#[async_trait]
impl Retriever for HybridRetriever {
    fn name(&self) -> &str {
        &self.name
    }

    async fn retrieve(&self, query: &str, top_k: usize) -> Result<Vec<ScoredDoc>, RetrieveError> {
        // Pull a pool at least as deep as the requested cutoff from each half so
        // fusion has material to rescue a doc one retriever ranked poorly.
        let pool = self.candidate_pool.max(top_k);
        let dense = self.dense.retrieve(query, pool).await?;
        let lexical = self.lexical.retrieve(query, pool).await?;
        Ok(reciprocal_rank_fusion(
            vec![dense, lexical],
            self.rrf_k,
            top_k,
        ))
    }
}

/// A deterministic test double: returns a fixed ranked list per query, so harness
/// behaviour (and hybrid-beats-singles demonstrations) can be exercised without a
/// real embedder or index. Unknown queries return an empty list.
pub struct PlantedRetriever {
    name: String,
    plan: std::collections::HashMap<String, Vec<String>>,
}

impl PlantedRetriever {
    /// Build a planted retriever from `(query -> ranked doc_ids)` entries.
    pub fn new(
        name: impl Into<String>,
        plan: std::collections::HashMap<String, Vec<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            plan,
        }
    }
}

#[async_trait]
impl Retriever for PlantedRetriever {
    fn name(&self) -> &str {
        &self.name
    }

    async fn retrieve(&self, query: &str, top_k: usize) -> Result<Vec<ScoredDoc>, RetrieveError> {
        let ranked = self.plan.get(query).cloned().unwrap_or_default();
        let n = ranked.len();
        Ok(ranked
            .into_iter()
            .take(top_k)
            .enumerate()
            // Descending synthetic score preserves the planted order through RRF.
            .map(|(i, id)| ScoredDoc::new(id, (n - i) as f64))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ardur_embeddings::MockEmbedder;

    fn docs() -> Vec<EvalDoc> {
        use crate::corpus::DocKind;
        vec![
            EvalDoc {
                id: "d1".into(),
                text: "the server binds port 8080 by default".into(),
                kind: DocKind::Note,
                stale: false,
            },
            EvalDoc {
                id: "d2".into(),
                text: "lazy dog sleeping under the porch".into(),
                kind: DocKind::Note,
                stale: false,
            },
        ]
    }

    #[tokio::test]
    async fn bm25_finds_exact_term() {
        let r = Bm25Retriever::index(&docs()).await.unwrap();
        let hits = r
            .retrieve("what port does the server bind?", 5)
            .await
            .unwrap();
        assert_eq!(hits[0].doc_id, "d1", "exact term should top BM25");
    }

    #[tokio::test]
    async fn dense_retriever_is_deterministic_and_ranks_all() {
        let r = DenseRetriever::index(MockEmbedder::new(64), &docs())
            .await
            .unwrap();
        let a = ranked_ids(&r.retrieve("server port", 5).await.unwrap());
        let b = ranked_ids(&r.retrieve("server port", 5).await.unwrap());
        assert_eq!(a, b, "dense retrieval must be deterministic");
        assert_eq!(a.len(), 2);
    }

    #[tokio::test]
    async fn hybrid_fuses_both_and_rescues_a_doc() {
        // dense plants d2 first, lexical plants d1 first → RRF puts the
        // doubly-present-nowhere docs by rank; d1 and d2 each rank #1 in one list
        // so both tie at 1/(0+60), broken by doc_id → d1, d2.
        let mut dplan = std::collections::HashMap::new();
        dplan_insert(&mut dplan, "q", &["d2", "d1"]);
        let mut lplan = std::collections::HashMap::new();
        dplan_insert(&mut lplan, "q", &["d1", "d2"]);
        let hybrid = HybridRetriever::new(
            Arc::new(PlantedRetriever::new("dense", dplan)),
            Arc::new(PlantedRetriever::new("bm25", lplan)),
        );
        let hits = ranked_ids(&hybrid.retrieve("q", 5).await.unwrap());
        assert_eq!(hits, vec!["d1".to_string(), "d2".to_string()]);
    }

    fn dplan_insert(m: &mut std::collections::HashMap<String, Vec<String>>, q: &str, ids: &[&str]) {
        m.insert(q.to_string(), ids.iter().map(|s| s.to_string()).collect());
    }
}
