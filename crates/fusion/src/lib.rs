//! ardur-fusion — rank/score fusion for hybrid retrieval.
//!
//! Hybrid retrieval runs the same query through several retrievers (a dense
//! embedding ANN search and a sparse BM25 lexical search, for example) and then
//! has to *combine* their ranked result lists into one. The retrievers score on
//! incomparable scales — cosine similarity in `[-1, 1]`, BM25 in unbounded
//! `[0, ∞)` — so you cannot just add their scores. This crate provides the three
//! fusion strategies LlamaIndex's `QueryFusionRetriever` ships, ported faithfully
//! to Rust:
//!
//! - [`reciprocal_rank_fusion`] — rank-based; ignores raw scores entirely and
//!   fuses on position. The robust default.
//! - [`relative_score_fusion`] — min-max normalize each list to `[0, 1]`, weight,
//!   and sum.
//! - [`distance_score_fusion`] — like relative-score but normalizes on a
//!   `mean ± 3σ` band instead of the observed min/max, which is steadier when a
//!   single outlier score would otherwise pin the min/max.
//!
//! # Source
//!
//! Ported from LlamaIndex `llama_index/core/retrievers/fusion_retriever.py`
//! (`_reciprocal_rerank_fusion`, `_relative_score_fusion`, and its `dist_based`
//! branch). Reciprocal-rank fusion itself is Cormack, Clarke & Büttcher,
//! *"Reciprocal Rank Fusion outperforms Condorcet and individual Rank Learning
//! Methods"*, SIGIR 2009 — which is also where the default `k = 60` comes from.
//!
//! This crate is pure (no I/O, no async); it operates on already-retrieved
//! [`ScoredDoc`] lists.
#![forbid(unsafe_code)]

use std::cmp::Ordering;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// LlamaIndex / Cormack-2009 default RRF constant. Larger `k` flattens the
/// contribution of top ranks (less weight on being #1); `60` is the value the
/// paper found robust across TREC runs.
pub const DEFAULT_RRF_K: f64 = 60.0;

/// A document id paired with a retriever's score for it.
///
/// `score` carries whatever scale the producing retriever uses (a BM25 score, a
/// cosine similarity, a fused score). Fusion functions only ever *rank* or
/// *normalize* these, never interpret the absolute magnitude across lists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoredDoc {
    /// Stable identifier for the document (matched across retriever lists).
    pub doc_id: String,
    /// The producing retriever's score for this document.
    pub score: f64,
}

impl ScoredDoc {
    /// Convenience constructor.
    pub fn new(doc_id: impl Into<String>, score: f64) -> Self {
        Self {
            doc_id: doc_id.into(),
            score,
        }
    }
}

/// Sort a fused map into a descending result list with a deterministic tie-break.
///
/// Ties on score break on `doc_id` ascending, so the output is identical run to
/// run regardless of `HashMap` iteration order — which matters for reproducible
/// tests and stable receipts.
fn finalize(fused: HashMap<String, f64>, top_k: usize) -> Vec<ScoredDoc> {
    let mut out: Vec<ScoredDoc> = fused
        .into_iter()
        .map(|(doc_id, score)| ScoredDoc { doc_id, score })
        .collect();
    out.sort_by(|a, b| match b.score.partial_cmp(&a.score) {
        Some(Ordering::Equal) | None => a.doc_id.cmp(&b.doc_id),
        Some(ord) => ord,
    });
    out.truncate(top_k);
    out
}

/// Rank a single list by score descending, returning indices into it.
///
/// Uses a stable sort so documents with equal scores keep their original input
/// order (matching LlamaIndex, which relies on Python's stable `sorted`).
fn ranked_indices(list: &[ScoredDoc]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..list.len()).collect();
    idx.sort_by(|&a, &b| {
        list[b]
            .score
            .partial_cmp(&list[a].score)
            .unwrap_or(Ordering::Equal)
    });
    idx
}

/// Reciprocal Rank Fusion (RRF).
///
/// For each result list, documents are ranked by score descending; a document at
/// 0-based `rank` contributes `1 / (rank + k)` to its fused score. Contributions
/// are summed across all lists, so a document that several retrievers rank highly
/// floats to the top. Raw scores are used only to establish rank within a list —
/// never compared across lists — which is what makes RRF robust to retrievers
/// scoring on wildly different scales.
///
/// Port of `_reciprocal_rerank_fusion` in LlamaIndex's `fusion_retriever.py`.
/// `k` defaults to [`DEFAULT_RRF_K`] (60.0) per Cormack 2009.
///
/// ```
/// use ardur_fusion::{reciprocal_rank_fusion, ScoredDoc, DEFAULT_RRF_K};
///
/// let dense = vec![ScoredDoc::new("a", 0.9), ScoredDoc::new("b", 0.5)];
/// let sparse = vec![ScoredDoc::new("b", 12.0), ScoredDoc::new("c", 3.0)];
/// let fused = reciprocal_rank_fusion(vec![dense, sparse], DEFAULT_RRF_K, 10);
/// // "b" is the only doc ranked by both retrievers, so it wins.
/// assert_eq!(fused[0].doc_id, "b");
/// ```
pub fn reciprocal_rank_fusion(
    result_lists: Vec<Vec<ScoredDoc>>,
    k: f64,
    top_k: usize,
) -> Vec<ScoredDoc> {
    let mut fused: HashMap<String, f64> = HashMap::new();
    for list in &result_lists {
        for (rank, &i) in ranked_indices(list).iter().enumerate() {
            let contribution = 1.0 / (rank as f64 + k);
            // Reuse the existing accumulator when this doc was already surfaced by
            // another list — the overlap RRF exists to reward, and the common case
            // for dense+sparse hybrid recall. Only clone the `doc_id` into the map
            // on first insertion, never on a repeat hit. Behaviour is identical to
            // `*fused.entry(id.clone()).or_insert(0.0) += contribution`.
            if let Some(acc) = fused.get_mut(&list[i].doc_id) {
                *acc += contribution;
            } else {
                fused.insert(list[i].doc_id.clone(), contribution);
            }
        }
    }
    finalize(fused, top_k)
}

/// The `(min, max)` band a list is normalized against in score fusion.
enum Band {
    /// Observed `min`/`max` of the list (relative-score fusion).
    MinMax,
    /// `mean ± 3σ` band (distance-score fusion).
    Sigma,
}

fn band(scores: &[f64], kind: &Band) -> (f64, f64) {
    match kind {
        Band::MinMax => {
            let mut lo = f64::INFINITY;
            let mut hi = f64::NEG_INFINITY;
            for &s in scores {
                lo = lo.min(s);
                hi = hi.max(s);
            }
            (lo, hi)
        }
        Band::Sigma => {
            let n = scores.len() as f64;
            let mean = scores.iter().sum::<f64>() / n;
            let var = scores.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / n;
            let std = var.sqrt();
            (mean - 3.0 * std, mean + 3.0 * std)
        }
    }
}

/// Shared body of relative-score and distance-score fusion: per-list min-max
/// normalize against `kind`'s band, scale by the list's weight, and sum per doc.
fn score_fusion(
    result_lists: Vec<Vec<ScoredDoc>>,
    weights: Option<Vec<f64>>,
    top_k: usize,
    kind: Band,
) -> Vec<ScoredDoc> {
    let weights = weights.unwrap_or_else(|| vec![1.0; result_lists.len()]);
    let mut fused: HashMap<String, f64> = HashMap::new();
    for (li, list) in result_lists.iter().enumerate() {
        if list.is_empty() {
            continue;
        }
        let weight = weights.get(li).copied().unwrap_or(1.0);
        let scores: Vec<f64> = list.iter().map(|d| d.score).collect();
        let (lo, hi) = band(&scores, &kind);
        let span = hi - lo;
        for doc in list {
            // Degenerate band (all scores equal, or σ = 0): map every doc to the
            // top of the range (1.0), matching LlamaIndex's div-by-zero guard.
            let norm = if span == 0.0 {
                1.0
            } else {
                ((doc.score - lo) / span).clamp(0.0, 1.0)
            };
            *fused.entry(doc.doc_id.clone()).or_insert(0.0) += norm * weight;
        }
    }
    finalize(fused, top_k)
}

/// Relative-score fusion: min-max normalize each list to `[0, 1]`, weight, sum.
///
/// Each retriever's list is normalized against its own observed min/max so the
/// best doc in every list scores `1.0` and the worst `0.0`; the normalized scores
/// are scaled by `weights[i]` (default all `1.0`) and summed per document. Unlike
/// RRF this keeps the *shape* of a retriever's score distribution (a doc that
/// barely edges out the rest still scores near the runner-up), at the cost of
/// being sensitive to outliers in the raw scores.
///
/// Port of `_relative_score_fusion` (the `dist_based=False` path) in LlamaIndex's
/// `fusion_retriever.py`. `weights`, when supplied, must be one weight per result
/// list; a missing entry defaults to `1.0`.
pub fn relative_score_fusion(
    result_lists: Vec<Vec<ScoredDoc>>,
    weights: Option<Vec<f64>>,
    top_k: usize,
) -> Vec<ScoredDoc> {
    score_fusion(result_lists, weights, top_k, Band::MinMax)
}

/// Distance-score fusion: normalize each list against a `mean ± 3σ` band, sum.
///
/// Identical to [`relative_score_fusion`] except the normalization band is
/// `[mean - 3σ, mean + 3σ]` rather than the observed `[min, max]`. Using the
/// distribution's spread instead of its extremes keeps a single anomalous score
/// from compressing every other doc toward 0, which is the steadier choice when
/// retriever scores are noisy.
///
/// Port of the `dist_based=True` branch of `_relative_score_fusion` in
/// LlamaIndex's `fusion_retriever.py`.
pub fn distance_score_fusion(result_lists: Vec<Vec<ScoredDoc>>, top_k: usize) -> Vec<ScoredDoc> {
    score_fusion(result_lists, None, top_k, Band::Sigma)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(docs: &[ScoredDoc]) -> Vec<&str> {
        docs.iter().map(|d| d.doc_id.as_str()).collect()
    }

    #[test]
    fn rrf_with_single_list_preserves_order() {
        let list = vec![
            ScoredDoc::new("a", 0.9),
            ScoredDoc::new("b", 0.5),
            ScoredDoc::new("c", 0.1),
        ];
        let out = reciprocal_rank_fusion(vec![list], DEFAULT_RRF_K, 10);
        assert_eq!(ids(&out), vec!["a", "b", "c"]);
        // With one list, fused score is exactly 1/(rank + k).
        assert!((out[0].score - 1.0 / 60.0).abs() < 1e-12);
        assert!((out[1].score - 1.0 / 61.0).abs() < 1e-12);
    }

    #[test]
    fn rrf_two_disjoint_lists_combines_correctly() {
        let l1 = vec![ScoredDoc::new("a", 0.9), ScoredDoc::new("b", 0.4)];
        let l2 = vec![ScoredDoc::new("c", 5.0), ScoredDoc::new("d", 1.0)];
        let out = reciprocal_rank_fusion(vec![l1, l2], DEFAULT_RRF_K, 10);
        assert_eq!(out.len(), 4);
        // Each doc appears once; rank-0 docs (a, c) tie above rank-1 docs (b, d).
        // Deterministic tie-break is doc_id ascending.
        assert_eq!(ids(&out), vec!["a", "c", "b", "d"]);
        assert!((out[0].score - 1.0 / 60.0).abs() < 1e-12);
    }

    #[test]
    fn rrf_two_overlapping_lists_boosts_common_docs() {
        // "b" is rank-1 in list 1 and rank-0 in list 2; "a" is rank-0 only once.
        let l1 = vec![ScoredDoc::new("a", 0.9), ScoredDoc::new("b", 0.4)];
        let l2 = vec![ScoredDoc::new("b", 5.0), ScoredDoc::new("e", 1.0)];
        let out = reciprocal_rank_fusion(vec![l1, l2], DEFAULT_RRF_K, 10);
        // b: 1/61 + 1/60; a: 1/60; e: 1/61. So b wins despite never being alone at top.
        assert_eq!(out[0].doc_id, "b");
        let b = 1.0 / 61.0 + 1.0 / 60.0;
        assert!((out[0].score - b).abs() < 1e-12);
        assert_eq!(ids(&out), vec!["b", "a", "e"]);
    }

    #[test]
    fn relative_score_fusion_normalizes_per_list() {
        // List 1 scores [10, 0] -> normalized [1, 0]. List 2 scores [100, 50] ->
        // normalized [1, 0]. Despite raw scale differing 10x, both top docs get 1.0.
        let l1 = vec![ScoredDoc::new("a", 10.0), ScoredDoc::new("b", 0.0)];
        let l2 = vec![ScoredDoc::new("c", 100.0), ScoredDoc::new("d", 50.0)];
        let out = relative_score_fusion(vec![l1, l2], None, 10);
        // a and c both normalize to 1.0; b and d to 0.0. Tie-break doc_id asc.
        assert_eq!(ids(&out), vec!["a", "c", "b", "d"]);
        assert!((out[0].score - 1.0).abs() < 1e-12);
        assert!((out[2].score - 0.0).abs() < 1e-12);
    }

    #[test]
    fn relative_score_fusion_weights_and_sums_overlap() {
        // Same doc "a" top of both lists -> 1.0 * w0 + 1.0 * w1.
        let l1 = vec![ScoredDoc::new("a", 10.0), ScoredDoc::new("b", 0.0)];
        let l2 = vec![ScoredDoc::new("a", 4.0), ScoredDoc::new("c", 1.0)];
        let out = relative_score_fusion(vec![l1, l2], Some(vec![0.7, 0.3]), 10);
        assert_eq!(out[0].doc_id, "a");
        assert!((out[0].score - (0.7 + 0.3)).abs() < 1e-12);
    }

    #[test]
    fn dist_score_fusion_normalizes_to_unit_scale() {
        // Symmetric list: mean is centered, every normalized score lands in [0, 1].
        let l1 = vec![
            ScoredDoc::new("a", 3.0),
            ScoredDoc::new("b", 2.0),
            ScoredDoc::new("c", 1.0),
        ];
        let out = distance_score_fusion(vec![l1], 10);
        assert_eq!(out[0].doc_id, "a");
        for d in &out {
            assert!(
                d.score >= 0.0 && d.score <= 1.0,
                "score {} out of unit scale",
                d.score
            );
        }
        // mean=2, std=sqrt(2/3); band = 2 ± 3*0.8165 = [-0.449, 4.449], span≈4.899.
        // a -> (3 - -0.449)/4.899 ≈ 0.7041.
        let mean = 2.0;
        let std = (2.0f64 / 3.0).sqrt();
        let lo = mean - 3.0 * std;
        let span = 6.0 * std;
        let expect_a = (3.0 - lo) / span;
        assert!((out[0].score - expect_a).abs() < 1e-9);
    }

    #[test]
    fn rrf_matches_llamaindex_reference_output() {
        // Reference case computed by hand against LlamaIndex's
        // `_reciprocal_rerank_fusion` (k = 60). Two retrievers:
        //   R1: [d1, d2, d3]   R2: [d2, d3, d4]
        // Fused scores (rank is 0-based):
        //   d1 = 1/60
        //   d2 = 1/61 (R1 rank 1) + 1/60 (R2 rank 0)
        //   d3 = 1/62 (R1 rank 2) + 1/61 (R2 rank 1)
        //   d4 = 1/62 (R2 rank 2)
        let r1 = vec![
            ScoredDoc::new("d1", 0.9),
            ScoredDoc::new("d2", 0.8),
            ScoredDoc::new("d3", 0.7),
        ];
        let r2 = vec![
            ScoredDoc::new("d2", 0.95),
            ScoredDoc::new("d3", 0.85),
            ScoredDoc::new("d4", 0.75),
        ];
        let out = reciprocal_rank_fusion(vec![r1, r2], 60.0, 10);

        let expect = |id: &str| -> f64 {
            match id {
                "d1" => 1.0 / 60.0,
                "d2" => 1.0 / 61.0 + 1.0 / 60.0,
                "d3" => 1.0 / 62.0 + 1.0 / 61.0,
                "d4" => 1.0 / 62.0,
                _ => unreachable!(),
            }
        };
        // Ranking by fused score: d2 > d3 > d1 > d4.
        assert_eq!(ids(&out), vec!["d2", "d3", "d1", "d4"]);
        for d in &out {
            assert!(
                (d.score - expect(&d.doc_id)).abs() < 1e-12,
                "doc {} score {} != {}",
                d.doc_id,
                d.score,
                expect(&d.doc_id)
            );
        }
    }

    #[test]
    fn top_k_truncates() {
        let l1 = vec![
            ScoredDoc::new("a", 0.9),
            ScoredDoc::new("b", 0.5),
            ScoredDoc::new("c", 0.1),
        ];
        let out = reciprocal_rank_fusion(vec![l1], DEFAULT_RRF_K, 2);
        assert_eq!(out.len(), 2);
        assert_eq!(ids(&out), vec!["a", "b"]);
    }

    #[test]
    fn empty_lists_fuse_to_empty() {
        assert!(reciprocal_rank_fusion(vec![], DEFAULT_RRF_K, 10).is_empty());
        assert!(reciprocal_rank_fusion(vec![vec![]], DEFAULT_RRF_K, 10).is_empty());
        assert!(relative_score_fusion(vec![vec![]], None, 10).is_empty());
        assert!(distance_score_fusion(vec![vec![]], 10).is_empty());
    }

    /// Property: fused output never exceeds `top_k`, nor the number of distinct
    /// input doc_ids. Exercised over a spread of shapes (proptest is not a
    /// workspace dependency, so this is a deterministic enumeration instead).
    #[test]
    fn fusion_top_k_never_exceeds_input() {
        for n_lists in 0..4usize {
            for len in 0..6usize {
                for top_k in 0..8usize {
                    let lists: Vec<Vec<ScoredDoc>> = (0..n_lists)
                        .map(|li| {
                            (0..len)
                                .map(|j| ScoredDoc::new(format!("l{li}d{j}"), (len - j) as f64))
                                .collect()
                        })
                        .collect();
                    let distinct = n_lists * len; // doc_ids are unique per (list, pos)
                    let bound = top_k.min(distinct);

                    let rrf = reciprocal_rank_fusion(lists.clone(), DEFAULT_RRF_K, top_k);
                    assert!(rrf.len() <= bound);
                    let rel = relative_score_fusion(lists.clone(), None, top_k);
                    assert!(rel.len() <= bound);
                    let dist = distance_score_fusion(lists, top_k);
                    assert!(dist.len() <= bound);
                }
            }
        }
    }
}
