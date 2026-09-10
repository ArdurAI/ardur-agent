//! The runner: drive a retriever over a golden set and aggregate the per-query
//! metrics into a [`MetricSummary`] / [`EvalReport`].
//!
//! One retrieval call per query (at the deepest cutoff) feeds every `@K` metric,
//! so a retriever is exercised exactly as production would exercise it.

use std::collections::BTreeMap;

use crate::corpus::GoldenSet;
use crate::metrics::{
    citation_correct_at_k, contradiction_handled_at_k, ndcg_at_k, precision_at_k, recall_at_k,
    reciprocal_rank_at_k, relevant_set, stale_hits_at_k,
};
use crate::report::{EvalReport, MetricSummary, ReleaseGate, RetrieverReport};
use crate::retriever::{RetrieveError, Retriever, ranked_ids};

/// How to run an evaluation.
#[derive(Debug, Clone)]
pub struct EvalConfig {
    /// The `@K` cutoffs to report Recall / Precision / nDCG / MRR at.
    pub cutoffs: Vec<usize>,
    /// The cutoff the single-value metrics (citation / stale / contradiction) and
    /// the release gate are reported at.
    pub primary_k: usize,
    /// The release gate to apply.
    pub gate: ReleaseGate,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self {
            cutoffs: vec![1, 3, 5, 10],
            primary_k: 5,
            gate: ReleaseGate::default(),
        }
    }
}

/// A running `(sum, count)` mean accumulator.
#[derive(Default, Clone, Copy)]
struct Mean {
    sum: f64,
    count: usize,
}

impl Mean {
    fn add(&mut self, v: f64) {
        self.sum += v;
        self.count += 1;
    }
    fn value(self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum / self.count as f64
        }
    }
    fn value_opt(self) -> Option<f64> {
        if self.count == 0 {
            None
        } else {
            Some(self.sum / self.count as f64)
        }
    }
}

/// Evaluate one retriever over `golden`, returning its aggregated metrics.
///
/// # Errors
///
/// Propagates any [`RetrieveError`] from the retriever.
pub async fn evaluate(
    retriever: &dyn Retriever,
    golden: &GoldenSet,
    config: &EvalConfig,
) -> Result<MetricSummary, RetrieveError> {
    let max_k = config
        .cutoffs
        .iter()
        .copied()
        .chain(std::iter::once(config.primary_k))
        .max()
        .unwrap_or(config.primary_k)
        .max(1);
    let stale_docs = golden.stale_doc_ids();

    let mut recall: BTreeMap<usize, Mean> = config
        .cutoffs
        .iter()
        .map(|&k| (k, Mean::default()))
        .collect();
    let mut precision = recall.clone();
    let mut ndcg = recall.clone();
    let mut mrr = recall.clone();

    let mut citation = Mean::default();
    let mut contradiction = Mean::default();
    let mut stale_hits = 0usize;
    let mut stale_total = 0usize;
    let mut queries_scored = 0usize;

    for q in &golden.queries {
        let hits = retriever.retrieve(&q.query, max_k).await?;
        let ranked = ranked_ids(&hits);

        // Ranking metrics only count queries that have ground truth.
        let has_ground_truth = !relevant_set(&q.relevant).is_empty();
        if has_ground_truth {
            queries_scored += 1;
            for &k in &config.cutoffs {
                if let Some(r) = recall_at_k(&ranked, &q.relevant, k) {
                    recall.get_mut(&k).unwrap().add(r);
                }
                if let Some(p) = precision_at_k(&ranked, &q.relevant, k) {
                    precision.get_mut(&k).unwrap().add(p);
                }
                if let Some(n) = ndcg_at_k(&ranked, &q.relevant, k) {
                    ndcg.get_mut(&k).unwrap().add(n);
                }
                // MRR counts every ground-truth query — a miss contributes 0.
                mrr.get_mut(&k)
                    .unwrap()
                    .add(reciprocal_rank_at_k(&ranked, &q.relevant, k));
            }
        }

        // Citation correctness (only citation-bearing queries).
        if let Some(ok) = citation_correct_at_k(&ranked, &q.expected_citations, config.primary_k) {
            citation.add(if ok { 1.0 } else { 0.0 });
        }

        // Stale-memory rate: pooled over every query's top-primary_k.
        let (s, t) = stale_hits_at_k(&ranked, &stale_docs, config.primary_k);
        stale_hits += s;
        stale_total += t;

        // Contradiction handling (only contradiction-bearing queries).
        if let Some(pair) = &q.contradiction {
            let handled = contradiction_handled_at_k(&ranked, pair, config.primary_k);
            contradiction.add(if handled { 1.0 } else { 0.0 });
        }
    }

    Ok(MetricSummary {
        retriever: retriever.name().to_string(),
        queries_scored,
        primary_k: config.primary_k,
        recall_at_k: recall.into_iter().map(|(k, m)| (k, m.value())).collect(),
        precision_at_k: precision.into_iter().map(|(k, m)| (k, m.value())).collect(),
        ndcg_at_k: ndcg.into_iter().map(|(k, m)| (k, m.value())).collect(),
        mrr_at_k: mrr.into_iter().map(|(k, m)| (k, m.value())).collect(),
        citation_correctness: citation.value_opt(),
        stale_memory_rate: if stale_total == 0 {
            0.0
        } else {
            stale_hits as f64 / stale_total as f64
        },
        contradiction_handling_rate: contradiction.value_opt(),
    })
}

/// Evaluate several retrievers over the same golden set into one comparable
/// [`EvalReport`] — the dense/BM25/hybrid baseline comparison.
///
/// # Errors
///
/// Propagates the first [`RetrieveError`] encountered.
pub async fn evaluate_all(
    retrievers: &[&dyn Retriever],
    golden: &GoldenSet,
    config: &EvalConfig,
) -> Result<EvalReport, RetrieveError> {
    let mut reports = Vec::with_capacity(retrievers.len());
    for r in retrievers {
        let summary = evaluate(*r, golden, config).await?;
        let verdict = summary.gate_verdict(&config.gate);
        reports.push(RetrieverReport { summary, verdict });
    }
    Ok(EvalReport {
        golden_set: golden.name.clone(),
        cutoffs: config.cutoffs.clone(),
        gate: config.gate,
        retrievers: reports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{DocKind, EvalDoc, GoldenQuery, QueryType};
    use crate::metrics::ContradictionPair;
    use crate::retriever::PlantedRetriever;
    use std::collections::HashMap;

    fn planted(entries: &[(&str, &[&str])]) -> PlantedRetriever {
        let mut m: HashMap<String, Vec<String>> = HashMap::new();
        for (q, ids) in entries {
            m.insert(
                (*q).to_string(),
                ids.iter().map(|s| (*s).to_string()).collect(),
            );
        }
        PlantedRetriever::new("planted", m)
    }

    fn golden() -> GoldenSet {
        GoldenSet {
            name: "runner-test".into(),
            docs: vec![
                EvalDoc {
                    id: "d1".into(),
                    text: "a".into(),
                    kind: DocKind::Note,
                    stale: false,
                },
                EvalDoc {
                    id: "d2".into(),
                    text: "b".into(),
                    kind: DocKind::Note,
                    stale: true,
                },
                EvalDoc {
                    id: "d3".into(),
                    text: "c".into(),
                    kind: DocKind::Note,
                    stale: false,
                },
            ],
            queries: vec![GoldenQuery {
                id: "q1".into(),
                query: "q".into(),
                query_type: QueryType::Factoid,
                relevant: [("d1".to_string(), 3u8)].into_iter().collect(),
                expected_citations: ["d1".to_string()].into_iter().collect(),
                contradiction: Some(ContradictionPair {
                    current: "d1".into(),
                    superseded: "d2".into(),
                }),
            }],
        }
    }

    #[tokio::test]
    async fn perfect_retriever_scores_perfectly() {
        // Retriever ranks the relevant doc d1 first, stale d2 last.
        let r = planted(&[("q", &["d1", "d3", "d2"])]);
        let cfg = EvalConfig::default();
        let s = evaluate(&r, &golden(), &cfg).await.unwrap();
        assert_eq!(s.queries_scored, 1);
        assert_eq!(s.recall(5), 1.0);
        assert_eq!(s.mrr(5), 1.0);
        assert_eq!(s.citation_correctness, Some(1.0));
        assert_eq!(s.contradiction_handling_rate, Some(1.0));
        // top-5 has one stale doc (d2) out of 3 retrieved → 1/3.
        assert!((s.stale_memory_rate - 1.0 / 3.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn bad_retriever_surfaces_stale_and_misses_citation() {
        // Ranks stale d2 first, never surfaces relevant d1.
        let r = planted(&[("q", &["d2", "d3"])]);
        let s = evaluate(&r, &golden(), &EvalConfig::default())
            .await
            .unwrap();
        assert_eq!(s.recall(5), 0.0);
        assert_eq!(s.mrr(5), 0.0);
        assert_eq!(s.citation_correctness, Some(0.0));
        assert_eq!(s.contradiction_handling_rate, Some(0.0)); // current absent
        assert!((s.stale_memory_rate - 0.5).abs() < 1e-9); // 1 stale of 2.
    }

    #[tokio::test]
    async fn evaluate_all_applies_gate() {
        let good = planted(&[("q", &["d1"])]);
        let bad = planted(&[("q", &["d3"])]);
        let report = evaluate_all(
            &[&good as &dyn Retriever, &bad as &dyn Retriever],
            &golden(),
            &EvalConfig::default(),
        )
        .await
        .unwrap();
        assert!(report.get("planted").is_some());
        // Both retrievers share the name "planted"; the first (good) passes the
        // gate, the second (bad) fails it.
        assert!(report.retrievers[0].verdict.is_pass());
        assert!(!report.retrievers[1].verdict.is_pass());
    }
}
