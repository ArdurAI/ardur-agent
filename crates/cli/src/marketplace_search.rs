//! Real local search over installed skills/plugins for `ardur marketplace
//! search`, replacing the earlier plain-substring filter.
//!
//! Two layers, both operating only over the **local** catalog of
//! already-installed skills/plugins — there is still no remote marketplace
//! index to search (a pre-existing, documented limitation this module does
//! not change):
//!
//! - **BM25 lexical search** ([`bm25_search`], the default): zero external
//!   services, zero network, deterministic, fast. Backed by
//!   [`ardur_bm25_index::Bm25Index`], already used elsewhere in this
//!   workspace's hybrid retriever.
//! - **Semantic search** ([`semantic_rerank`], opt-in via `--semantic`):
//!   cosine-similarity ranking over a locally-computed embedding
//!   ([`ardur_embeddings::FastEmbedEmbedder`]), which downloads its ONNX
//!   model on first use (no per-query network call after that). Kept opt-in
//!   rather than the default so a plain `marketplace search` never triggers
//!   an unexpected model download.
//!
//! The corpus size fed to either layer is bounded
//! ([`MAX_SEARCH_CORPUS`]) — installed-skill counts are naturally small, but
//! the ceiling guards the pathological case regardless.

use ardur_bm25_index::Bm25Index;
use ardur_cli::CliError;
use ardur_embeddings::{Embedder, FastEmbedEmbedder};

/// Hard ceiling on the number of installed records fed into a search index —
/// installed-skill counts are naturally small, but this guards the
/// pathological case.
const MAX_SEARCH_CORPUS: usize = 5_000;

/// One scored hit.
#[derive(Debug)]
pub(crate) struct SearchHit {
    pub skill_id: String,
    pub score: f32,
}

fn run_blocking<T>(
    fut: impl std::future::Future<Output = Result<T, CliError>>,
) -> Result<T, CliError> {
    tokio::runtime::Runtime::new()
        .map_err(|e| CliError::State(format!("starting search runtime: {e}")))?
        .block_on(fut)
}

/// BM25-search `corpus` (a `(doc_id, text)` pair per installed record)
/// against `query`, returning up to `top_k` hits ordered by score desc.
pub(crate) fn bm25_search(
    corpus: &[(String, String)],
    query: &str,
    top_k: usize,
) -> Result<Vec<SearchHit>, CliError> {
    if corpus.len() > MAX_SEARCH_CORPUS {
        return Err(CliError::State(format!(
            "search corpus is {} records, exceeding the {MAX_SEARCH_CORPUS} ceiling",
            corpus.len()
        )));
    }
    run_blocking(async move {
        let mut index = Bm25Index::new(None)
            .map_err(|e| CliError::State(format!("building BM25 index: {e}")))?;
        for (doc_id, text) in corpus {
            index
                .add(doc_id.clone(), text.clone())
                .await
                .map_err(|e| CliError::State(format!("indexing `{doc_id}`: {e}")))?;
        }
        let hits = index
            .query(query, top_k)
            .await
            .map_err(|e| CliError::State(format!("BM25 query failed: {e}")))?;
        Ok(hits
            .into_iter()
            .map(|h| SearchHit {
                skill_id: h.doc_id,
                score: h.score,
            })
            .collect())
    })
}

/// Semantic-search `corpus` against `query` via a locally-computed embedding
/// (downloads its model on first use — see the module docs).
pub(crate) fn semantic_rerank(
    corpus: &[(String, String)],
    query: &str,
    top_k: usize,
) -> Result<Vec<SearchHit>, CliError> {
    if corpus.len() > MAX_SEARCH_CORPUS {
        return Err(CliError::State(format!(
            "search corpus is {} records, exceeding the {MAX_SEARCH_CORPUS} ceiling",
            corpus.len()
        )));
    }
    run_blocking(async move {
        let embedder = FastEmbedEmbedder::from_env()
            .map_err(|e| CliError::State(format!("loading embedding model: {e}")))?;
        semantic_rerank_with(&embedder, corpus, query, top_k).await
    })
}

/// The embedder-generic core of [`semantic_rerank`], split out so tests can
/// exercise the ranking logic with [`ardur_embeddings::MockEmbedder`]
/// (deterministic, no model download) instead of the real model.
async fn semantic_rerank_with(
    embedder: &dyn Embedder,
    corpus: &[(String, String)],
    query: &str,
    top_k: usize,
) -> Result<Vec<SearchHit>, CliError> {
    if corpus.is_empty() {
        return Ok(Vec::new());
    }
    let mut texts: Vec<String> = corpus.iter().map(|(_, text)| text.clone()).collect();
    texts.push(query.to_string());
    let mut vectors = embedder
        .embed(texts)
        .await
        .map_err(|e| CliError::State(format!("computing embeddings: {e}")))?;
    let query_vector = vectors
        .pop()
        .ok_or_else(|| CliError::State("embedder returned no vectors".to_string()))?;

    // Both fastembed and MockEmbedder L2-normalize their output, so cosine
    // similarity reduces to a plain dot product.
    let mut scored: Vec<SearchHit> = corpus
        .iter()
        .zip(vectors.iter())
        .map(|((skill_id, _), vector)| SearchHit {
            skill_id: skill_id.clone(),
            score: dot(vector, &query_vector),
        })
        .collect();
    scored.sort_by(|a, b| b.score.total_cmp(&a.score));
    scored.truncate(top_k);
    Ok(scored)
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ardur_embeddings::MockEmbedder;

    #[tokio::test]
    async fn semantic_rerank_ranks_closer_text_higher() {
        let embedder = MockEmbedder::new(32);
        let corpus = vec![
            ("skill.alpha".to_string(), "alpha alpha alpha".to_string()),
            (
                "skill.beta".to_string(),
                "completely unrelated text".to_string(),
            ),
        ];
        let hits = semantic_rerank_with(&embedder, &corpus, "alpha alpha alpha", 10)
            .await
            .expect("semantic rerank succeeds");
        assert_eq!(hits.len(), 2);
        // The mock embedder is deterministic per exact text, so the query
        // text that exactly matches skill.alpha's indexed text should score
        // as an exact (self-)match — the highest possible cosine score.
        assert_eq!(hits[0].skill_id, "skill.alpha");
        assert!(hits[0].score > hits[1].score);
    }

    #[tokio::test]
    async fn semantic_rerank_respects_top_k() {
        let embedder = MockEmbedder::new(16);
        let corpus: Vec<(String, String)> = (0..5)
            .map(|i| (format!("skill.{i}"), format!("text number {i}")))
            .collect();
        let hits = semantic_rerank_with(&embedder, &corpus, "text", 2)
            .await
            .expect("semantic rerank succeeds");
        assert_eq!(hits.len(), 2);
    }

    #[tokio::test]
    async fn semantic_rerank_empty_corpus_returns_empty() {
        let embedder = MockEmbedder::new(16);
        let hits = semantic_rerank_with(&embedder, &[], "anything", 10)
            .await
            .expect("semantic rerank succeeds");
        assert!(hits.is_empty());
    }

    #[test]
    fn bm25_search_finds_lexical_match_and_ranks_by_relevance() {
        let corpus = vec![
            (
                "skill.helper".to_string(),
                "Helper skill.helper cap.fs_read".to_string(),
            ),
            (
                "skill.other".to_string(),
                "Other skill.other cap.network_out".to_string(),
            ),
        ];
        let hits = bm25_search(&corpus, "helper", 10).expect("bm25 search succeeds");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].skill_id, "skill.helper");
    }

    #[test]
    fn bm25_search_rejects_oversized_corpus() {
        let corpus: Vec<(String, String)> = (0..(MAX_SEARCH_CORPUS + 1))
            .map(|i| (format!("skill.{i}"), "text".to_string()))
            .collect();
        let err = bm25_search(&corpus, "text", 10).expect_err("oversized corpus is refused");
        assert!(matches!(err, CliError::State(_)));
    }
}
