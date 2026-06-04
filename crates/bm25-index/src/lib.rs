//! ardur-bm25-index — BM25 lexical search over a Tantivy index.
//!
//! The sparse (lexical) half of hybrid retrieval. Where a dense embedding search
//! matches on *meaning*, BM25 matches on *terms* — it rewards documents that
//! contain the query's exact words, weighting rarer words more and saturating on
//! term-frequency. The two are complementary, which is why hybrid retrieval runs
//! both and fuses the results (see the `ardur-fusion` crate).
//!
//! [`Bm25Index`] wraps a Tantivy index with a fixed two-field schema:
//!
//! - `doc_id` — `STRING | STORED | FAST`: the document's stable id, stored so a
//!   hit can be mapped back to it, and `FAST` so it is cheap to read.
//! - `text` — `TEXT`: the searchable body, tokenized and inverted (not stored —
//!   we only ever need it for matching, never to read back).
//!
//! Construct in-memory ([`Bm25Index::new`] with `None`) for ephemeral / test use,
//! or file-backed ([`Bm25Index::new`] with `Some(dir)`) to persist across process
//! restarts.
//!
//! Tantivy is synchronous; the [`Bm25Index::add`] / [`Bm25Index::query`] methods
//! are `async` only to mirror the `Embedder` surface in `ardur-embeddings` so a
//! hybrid retriever can `await` both halves uniformly.
#![forbid(unsafe_code)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::QueryParser;
use tantivy::schema::{FAST, Field, STORED, STRING, Schema, TEXT, TantivyDocument, Value};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyError};

/// Tantivy writer heap budget. 50 MB is Tantivy's documented sane floor for a
/// single-threaded writer; the BM25 index here is small (memory cache), so this
/// is comfortably more than enough.
const WRITER_HEAP_BYTES: usize = 50_000_000;

/// A document id paired with its BM25 relevance score for a query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoredDoc {
    /// The `doc_id` supplied at [`Bm25Index::add`] time.
    pub doc_id: String,
    /// Tantivy's BM25 score for this document against the query (higher = more
    /// relevant). Not comparable across queries.
    pub score: f32,
}

/// Errors from opening, indexing into, or querying a [`Bm25Index`].
#[derive(Debug, thiserror::Error)]
pub enum Bm25Error {
    /// The index directory could not be created or opened.
    #[error("index directory I/O: {0}")]
    Directory(String),
    /// A query string failed to parse.
    #[error("query parse error: {0}")]
    QueryParse(String),
    /// An underlying Tantivy operation failed (commit, search, doc fetch).
    #[error("tantivy: {0}")]
    Tantivy(#[from] TantivyError),
}

/// A BM25 lexical index over `(doc_id, text)` documents.
///
/// Backed by Tantivy. Each [`add`](Bm25Index::add) commits, so a subsequent
/// [`query`](Bm25Index::query) sees the document — convenient for incremental use
/// and tests, at the cost of a commit per document (a batched writer is a future
/// optimization, not needed for the memory-store sizes this targets).
pub struct Bm25Index {
    index: Index,
    writer: IndexWriter,
    reader: IndexReader,
    doc_id_field: Field,
    text_field: Field,
}

impl Bm25Index {
    /// Build the fixed two-field BM25 schema (`doc_id`, `text`).
    fn schema() -> (Schema, Field, Field) {
        let mut builder = Schema::builder();
        let doc_id = builder.add_text_field("doc_id", STRING | STORED | FAST);
        let text = builder.add_text_field("text", TEXT);
        (builder.build(), doc_id, text)
    }

    /// Open a BM25 index.
    ///
    /// `None` builds an in-memory (RAM) index that vanishes on drop. `Some(dir)`
    /// builds (or reopens) a file-backed index under `dir` — the directory is
    /// created if absent, and an index already present there is reopened with its
    /// documents intact (see `persistent_index_survives_reopen`).
    pub fn new(index_dir: Option<PathBuf>) -> Result<Self, Bm25Error> {
        let (schema, doc_id_field, text_field) = Self::schema();

        let index = match index_dir {
            None => Index::create_in_ram(schema),
            Some(dir) => {
                std::fs::create_dir_all(&dir).map_err(|e| Bm25Error::Directory(e.to_string()))?;
                let mmap =
                    MmapDirectory::open(&dir).map_err(|e| Bm25Error::Directory(e.to_string()))?;
                // Reopen an existing index in the directory, or create one if the
                // directory is empty. `open_or_create` is idempotent across runs,
                // which is what makes the persistent index survive a reopen.
                Index::open_or_create(mmap, schema)?
            }
        };

        let writer: IndexWriter = index.writer(WRITER_HEAP_BYTES)?;
        // Manual reload policy + an explicit `reload()` after each commit gives a
        // deterministic read-your-writes view (the default on-commit policy is
        // asynchronous and can briefly lag a just-committed write).
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;

        Ok(Self {
            index,
            writer,
            reader,
            doc_id_field,
            text_field,
        })
    }

    /// Index a document under `doc_id`, then commit so it is immediately queryable.
    ///
    /// Re-adding an existing `doc_id` does not replace the prior document — Tantivy
    /// has no implicit upsert — so callers that update documents should track
    /// their own ids; for the append-mostly memory-store use this targets that is
    /// acceptable.
    pub async fn add(&mut self, doc_id: String, text: String) -> Result<(), Bm25Error> {
        let mut doc = TantivyDocument::default();
        doc.add_text(self.doc_id_field, &doc_id);
        doc.add_text(self.text_field, &text);
        self.writer.add_document(doc)?;
        self.writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    /// BM25-search `query`, returning up to `top_k` hits ordered by score desc.
    ///
    /// `query` is parsed against the `text` field with Tantivy's
    /// [`QueryParser`] (so it accepts bare terms and Tantivy's query syntax). An
    /// empty result is returned for a query that matches nothing.
    pub async fn query(&self, query: &str, top_k: usize) -> Result<Vec<ScoredDoc>, Bm25Error> {
        if top_k == 0 {
            return Ok(Vec::new());
        }
        let searcher = self.reader.searcher();
        let parser = QueryParser::for_index(&self.index, vec![self.text_field]);
        let parsed = parser
            .parse_query(query)
            .map_err(|e| Bm25Error::QueryParse(e.to_string()))?;

        let hits = searcher.search(&parsed, &TopDocs::with_limit(top_k))?;
        let mut out = Vec::with_capacity(hits.len());
        for (score, addr) in hits {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let doc_id = doc
                .get_first(self.doc_id_field)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            out.push(ScoredDoc { doc_id, score });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn add_then_query_returns_doc() {
        let mut idx = Bm25Index::new(None).unwrap();
        idx.add("doc-1".into(), "the quick brown fox".into())
            .await
            .unwrap();
        idx.add("doc-2".into(), "lazy sleeping dog".into())
            .await
            .unwrap();

        let hits = idx.query("fox", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc_id, "doc-1");
        assert!(hits[0].score > 0.0);

        // A term in neither document yields no hits.
        let none = idx.query("aardvark", 10).await.unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn query_higher_tf_scores_higher() {
        let mut idx = Bm25Index::new(None).unwrap();
        // doc-hi repeats "rust" more; BM25 term-frequency should rank it above
        // doc-lo (both docs are similar length so length-norm doesn't invert it).
        idx.add(
            "doc-hi".into(),
            "rust rust rust rust systems language".into(),
        )
        .await
        .unwrap();
        idx.add(
            "doc-lo".into(),
            "rust is one of many systems languages".into(),
        )
        .await
        .unwrap();

        let hits = idx.query("rust", 10).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].doc_id, "doc-hi");
        assert!(
            hits[0].score > hits[1].score,
            "higher tf {} should outscore lower tf {}",
            hits[0].score,
            hits[1].score
        );
    }

    #[tokio::test]
    async fn persistent_index_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();

        {
            let mut idx = Bm25Index::new(Some(path.clone())).unwrap();
            idx.add("persisted".into(), "durable tantivy document".into())
                .await
                .unwrap();
            // idx (and its writer) dropped here — the commit is already on disk.
        }

        // Reopen the same directory: the document is still searchable.
        let reopened = Bm25Index::new(Some(path)).unwrap();
        let hits = reopened.query("durable", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc_id, "persisted");
    }

    #[tokio::test]
    async fn top_k_zero_returns_empty() {
        let mut idx = Bm25Index::new(None).unwrap();
        idx.add("d".into(), "anything".into()).await.unwrap();
        assert!(idx.query("anything", 0).await.unwrap().is_empty());
    }
}
