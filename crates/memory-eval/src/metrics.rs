//! The information-retrieval metrics the harness reports, as pure functions over
//! a ranked result list and per-query relevance judgments.
//!
//! Every function here is deterministic and side-effect-free so the metric math
//! can be unit-tested against hand-computed values (see the `tests` module) —
//! the whole point of Finding 5 is that retrieval quality is judged by numbers
//! that are themselves trustworthy.
//!
//! # Conventions
//!
//! - A **ranked result** is a slice of `doc_id`s in rank order, best first.
//! - **Graded relevance** is a map `doc_id -> grade` where a higher grade means
//!   more relevant; grade `0` (or an absent doc) is non-relevant. A doc is
//!   *relevant* iff its grade is `>= 1`. Grades feed nDCG's gain; the binary
//!   relevant/not split feeds Recall and MRR.
//! - `k` is the cutoff (`@K`): only the first `k` results are considered. `k`
//!   larger than the result list just considers the whole list.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// A doc is relevant iff its graded relevance is at least this.
pub const RELEVANT_GRADE: u8 = 1;

/// The set of relevant `doc_id`s in `grades` (grade `>= RELEVANT_GRADE`).
pub fn relevant_set(grades: &HashMap<String, u8>) -> HashSet<&str> {
    grades
        .iter()
        .filter(|&(_, &g)| g >= RELEVANT_GRADE)
        .map(|(id, _)| id.as_str())
        .collect()
}

/// **Recall@K** — the fraction of all relevant docs that appear in the top `k`.
///
/// `|{relevant docs in ranked[..k]}| / |{all relevant docs}|`.
///
/// Returns `None` when there are no relevant docs for the query (recall is
/// undefined — a query with an empty ground truth must be *skipped*, not counted
/// as 0 or 1, or it silently biases the mean).
pub fn recall_at_k(ranked: &[String], grades: &HashMap<String, u8>, k: usize) -> Option<f64> {
    let relevant = relevant_set(grades);
    if relevant.is_empty() {
        return None;
    }
    let hit = ranked
        .iter()
        .take(k)
        .filter(|d| relevant.contains(d.as_str()))
        .count();
    Some(hit as f64 / relevant.len() as f64)
}

/// **Precision@K** — the fraction of the top `k` results that are relevant.
///
/// Reported alongside Recall for context. Uses `k` (not the result length) as
/// the denominator, the standard `P@k` convention, so a retriever returning
/// fewer than `k` docs is penalised for the short list.
pub fn precision_at_k(ranked: &[String], grades: &HashMap<String, u8>, k: usize) -> Option<f64> {
    if k == 0 {
        return None;
    }
    let relevant = relevant_set(grades);
    if relevant.is_empty() {
        return None;
    }
    let hit = ranked
        .iter()
        .take(k)
        .filter(|d| relevant.contains(d.as_str()))
        .count();
    Some(hit as f64 / k as f64)
}

/// **DCG@K** — Discounted Cumulative Gain with the industry-standard exponential
/// gain `2^grade - 1` and log-2 discount `1 / log2(rank + 1)` (1-based rank).
///
/// `sum over i in 0..k of (2^grade(ranked[i]) - 1) / log2(i + 2)`.
pub fn dcg_at_k(ranked: &[String], grades: &HashMap<String, u8>, k: usize) -> f64 {
    ranked
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, doc)| {
            let grade = grades.get(doc).copied().unwrap_or(0);
            let gain = (2f64).powi(i32::from(grade)) - 1.0;
            // 0-based position `i` is 1-based rank `i + 1`; the discount is
            // `1 / log2(rank + 1) = 1 / log2(i + 2)`.
            gain / ((i as f64) + 2.0).log2()
        })
        .sum()
}

/// **nDCG@K** — DCG@K normalized by the ideal DCG@K (the DCG of the best possible
/// ranking of the same judgments), so it lands in `[0, 1]`.
///
/// Returns `None` when the ideal DCG is 0 (no relevant docs — nDCG is undefined,
/// same skip rule as Recall).
pub fn ndcg_at_k(ranked: &[String], grades: &HashMap<String, u8>, k: usize) -> Option<f64> {
    let dcg = dcg_at_k(ranked, grades, k);
    // Ideal ranking: the graded docs sorted by grade descending.
    let mut ideal: Vec<u8> = grades.values().copied().filter(|&g| g > 0).collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let idcg: f64 = ideal
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, &grade)| ((2f64).powi(i32::from(grade)) - 1.0) / ((i as f64) + 2.0).log2())
        .sum();
    if idcg == 0.0 { None } else { Some(dcg / idcg) }
}

/// **Reciprocal Rank@K** — `1 / rank` of the first relevant doc in the top `k`
/// (1-based rank), or `0.0` if none of the top `k` is relevant.
///
/// The mean of this over a query set is **MRR@K**.
pub fn reciprocal_rank_at_k(ranked: &[String], grades: &HashMap<String, u8>, k: usize) -> f64 {
    let relevant = relevant_set(grades);
    for (i, doc) in ranked.iter().take(k).enumerate() {
        if relevant.contains(doc.as_str()) {
            return 1.0 / ((i as f64) + 1.0);
        }
    }
    0.0
}

/// **Citation correctness@K** — did the retriever surface a doc the answer should
/// cite within the top `k`?
///
/// `Some(true)` if any `expected_citations` doc is in `ranked[..k]`, `Some(false)`
/// if none is, `None` if the query has no citation expectation (skip). The mean
/// over citation-bearing queries is the citation-correctness rate.
pub fn citation_correct_at_k(
    ranked: &[String],
    expected_citations: &HashSet<String>,
    k: usize,
) -> Option<bool> {
    if expected_citations.is_empty() {
        return None;
    }
    Some(
        ranked
            .iter()
            .take(k)
            .any(|d| expected_citations.contains(d)),
    )
}

/// **Stale hits@K** — `(stale_in_topk, total_in_topk)` for one query: how many of
/// the top `k` results are stale (invalidated / superseded) memories, out of how
/// many results the top `k` actually contains.
///
/// A retriever that surfaces invalidated memories is actively harmful; summing
/// the two components across a query set yields the **stale-memory rate**
/// (`sum stale / sum total`), which should be near 0.
pub fn stale_hits_at_k(ranked: &[String], stale: &HashSet<String>, k: usize) -> (usize, usize) {
    let considered: Vec<&String> = ranked.iter().take(k).collect();
    let stale_hits = considered.iter().filter(|d| stale.contains(**d)).count();
    (stale_hits, considered.len())
}

/// One query's contradiction annotation: a `current` fact that supersedes a
/// `superseded` one asserting the opposite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContradictionPair {
    /// The `doc_id` of the current, correct fact.
    pub current: String,
    /// The `doc_id` of the superseded, contradicted fact.
    pub superseded: String,
}

/// **Contradiction handled@K** — for a query whose ground truth has a
/// contradiction pair, did the retriever rank the *current* fact above the
/// *superseded* one within the top `k`?
///
/// Handled (`true`) iff, within `ranked[..k]`, the current fact appears and the
/// superseded fact either does not appear or appears strictly after it. Not
/// handled (`false`) iff the superseded fact appears at or above the current one
/// (including the current being absent while the superseded is present). The mean
/// over contradiction-bearing queries is the contradiction-handling rate.
pub fn contradiction_handled_at_k(ranked: &[String], pair: &ContradictionPair, k: usize) -> bool {
    let pos = |target: &str| ranked.iter().take(k).position(|d| d == target);
    match (pos(&pair.current), pos(&pair.superseded)) {
        // Current present, superseded absent → handled.
        (Some(_), None) => true,
        // Both present → handled iff current ranks strictly higher.
        (Some(c), Some(s)) => c < s,
        // Current absent (superseded present or not) → not handled: the retriever
        // failed to surface the correct fact over the stale one.
        (None, _) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grades(pairs: &[(&str, u8)]) -> HashMap<String, u8> {
        pairs.iter().map(|(d, g)| ((*d).to_string(), *g)).collect()
    }

    fn ranked(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn recall_counts_relevant_in_topk() {
        // 4 relevant docs (a, b, c, d); top-3 contains a and c → 2/4.
        let g = grades(&[("a", 1), ("b", 2), ("c", 3), ("d", 1), ("x", 0)]);
        let r = ranked(&["a", "x", "c", "z"]);
        assert_eq!(recall_at_k(&r, &g, 3), Some(0.5));
        // At k=10 (whole list): a, c present → still 2/4 (b, d not retrieved).
        assert_eq!(recall_at_k(&r, &g, 10), Some(0.5));
    }

    #[test]
    fn recall_is_none_without_ground_truth() {
        let g = grades(&[("a", 0), ("b", 0)]);
        assert_eq!(recall_at_k(&ranked(&["a", "b"]), &g, 5), None);
    }

    #[test]
    fn precision_uses_k_denominator() {
        let g = grades(&[("a", 1), ("b", 1)]);
        // top-4 has 2 relevant (a, b) → 2/4 = 0.5 even though only 3 retrieved.
        let r = ranked(&["a", "x", "b"]);
        assert_eq!(precision_at_k(&r, &g, 4), Some(0.5));
    }

    #[test]
    fn dcg_matches_hand_computation() {
        // grades: a=3, b=2, c=0; ranked [a, c, b].
        // DCG = (2^3-1)/log2(2) + (2^0-1)/log2(3) + (2^2-1)/log2(4)
        //     = 7/1 + 0/1.585 + 3/2 = 7 + 0 + 1.5 = 8.5
        let g = grades(&[("a", 3), ("b", 2), ("c", 0)]);
        let r = ranked(&["a", "c", "b"]);
        let dcg = dcg_at_k(&r, &g, 3);
        assert!((dcg - 8.5).abs() < 1e-9, "dcg = {dcg}");
    }

    #[test]
    fn ndcg_of_ideal_ranking_is_one() {
        // Ideal order a(3), b(2), c(1) → nDCG = 1.
        let g = grades(&[("a", 3), ("b", 2), ("c", 1)]);
        let r = ranked(&["a", "b", "c"]);
        let n = ndcg_at_k(&r, &g, 3).unwrap();
        assert!((n - 1.0).abs() < 1e-9, "ndcg = {n}");
    }

    #[test]
    fn ndcg_penalises_bad_order() {
        // Reversed order c(1), b(2), a(3): DCG < IDCG so nDCG < 1.
        // DCG = 1/1 + 3/1.585 + 7/2 = 1 + 1.8927 + 3.5 = 6.3927
        // IDCG = 7/1 + 3/1.585 + 1/2 = 7 + 1.8927 + 0.5 = 9.3927
        // nDCG = 6.3927 / 9.3927 = 0.6806...
        let g = grades(&[("a", 3), ("b", 2), ("c", 1)]);
        let r = ranked(&["c", "b", "a"]);
        let n = ndcg_at_k(&r, &g, 3).unwrap();
        assert!((n - 0.680_58).abs() < 1e-4, "ndcg = {n}");
    }

    #[test]
    fn mrr_is_reciprocal_of_first_relevant_rank() {
        let g = grades(&[("a", 0), ("b", 2), ("c", 1)]);
        // first relevant is b at 1-based rank 2 → 1/2.
        assert_eq!(reciprocal_rank_at_k(&ranked(&["a", "b", "c"]), &g, 5), 0.5);
        // none relevant in top-1 → 0.
        assert_eq!(reciprocal_rank_at_k(&ranked(&["a", "b", "c"]), &g, 1), 0.0);
    }

    #[test]
    fn citation_correct_when_expected_in_topk() {
        let expected: HashSet<String> = ["src-42".to_string()].into_iter().collect();
        assert_eq!(
            citation_correct_at_k(&ranked(&["x", "src-42", "y"]), &expected, 3),
            Some(true)
        );
        assert_eq!(
            citation_correct_at_k(&ranked(&["x", "src-42", "y"]), &expected, 1),
            Some(false)
        );
        assert_eq!(
            citation_correct_at_k(&ranked(&["x"]), &HashSet::new(), 3),
            None
        );
    }

    #[test]
    fn stale_hits_counts_invalidated_in_topk() {
        let stale: HashSet<String> = ["old-1".to_string(), "old-2".to_string()]
            .into_iter()
            .collect();
        // top-3 = [fresh, old-1, fresh2] → 1 stale of 3.
        let (s, t) = stale_hits_at_k(&ranked(&["fresh", "old-1", "fresh2", "old-2"]), &stale, 3);
        assert_eq!((s, t), (1, 3));
    }

    #[test]
    fn contradiction_handled_when_current_ranks_higher() {
        let pair = ContradictionPair {
            current: "new".to_string(),
            superseded: "old".to_string(),
        };
        assert!(contradiction_handled_at_k(
            &ranked(&["new", "old"]),
            &pair,
            5
        ));
        assert!(!contradiction_handled_at_k(
            &ranked(&["old", "new"]),
            &pair,
            5
        ));
        // current present, superseded absent → handled.
        assert!(contradiction_handled_at_k(&ranked(&["new", "x"]), &pair, 5));
        // current absent, superseded present → not handled.
        assert!(!contradiction_handled_at_k(
            &ranked(&["old", "x"]),
            &pair,
            5
        ));
        // superseded ranked above current but outside k → within k only current
        // counts → handled.
        assert!(contradiction_handled_at_k(
            &ranked(&["new", "x", "old"]),
            &pair,
            2
        ));
    }
}
