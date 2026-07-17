//! The eval report: the aggregated metrics for each retriever over a golden set,
//! plus the release-gate verdict.
//!
//! The shape follows the cycle-3 `MEMORY-V3-EVAL-GATES-SCAFFOLD` convention (a
//! JSON-serializable report with an explicit pass/fail verdict) so retrieval
//! numbers slot into the same audit surface as the consolidation-correctness
//! gates, and so `graph-RAG-vs-baseline` comparisons diff cleanly.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The V3 §394 release-gate thresholds a hybrid retriever must clear to be
/// considered production-ready (and the bar graph-RAG must then beat by 1.10x per
/// Q5-a).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReleaseGate {
    /// The cutoff the gate is measured at (default 5).
    pub k: usize,
    /// Minimum mean Recall@k (default 0.70).
    pub min_recall: f64,
    /// Minimum mean MRR@k (default 0.55).
    pub min_mrr: f64,
}

impl Default for ReleaseGate {
    fn default() -> Self {
        Self {
            k: 5,
            min_recall: 0.70,
            min_mrr: 0.55,
        }
    }
}

/// Whether a retriever's numbers clear the release gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "verdict")]
pub enum GateVerdict {
    /// Both thresholds met.
    Pass,
    /// At least one threshold missed; carries the human-readable reasons.
    Fail {
        /// Why the gate failed (one entry per missed threshold).
        reasons: Vec<String>,
    },
}

impl GateVerdict {
    /// `true` iff this is [`GateVerdict::Pass`].
    pub fn is_pass(&self) -> bool {
        matches!(self, GateVerdict::Pass)
    }
}

/// The aggregated metrics for a single retriever over a golden set.
///
/// `recall_at_k` / `precision_at_k` / `ndcg_at_k` / `mrr_at_k` are means over the
/// eligible queries at each cutoff `K` (queries without ground truth are skipped,
/// never counted as 0). The single-value metrics are reported at [`primary_k`].
///
/// [`primary_k`]: MetricSummary::primary_k
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricSummary {
    /// The retriever's name (`"bm25"`, `"dense"`, `"hybrid-rrf"`).
    pub retriever: String,
    /// How many queries had ground truth and were scored.
    pub queries_scored: usize,
    /// The cutoff single-value metrics are reported at (default 5).
    pub primary_k: usize,
    /// Mean Recall@K per cutoff.
    pub recall_at_k: BTreeMap<usize, f64>,
    /// Mean Precision@K per cutoff.
    pub precision_at_k: BTreeMap<usize, f64>,
    /// Mean nDCG@K per cutoff.
    pub ndcg_at_k: BTreeMap<usize, f64>,
    /// Mean MRR@K per cutoff.
    pub mrr_at_k: BTreeMap<usize, f64>,
    /// Fraction of citation-bearing queries whose expected source was retrieved
    /// in the top [`primary_k`], or `None` if no query had a citation
    /// expectation.
    pub citation_correctness: Option<f64>,
    /// Fraction of the top-[`primary_k`] results (across all queries) that were
    /// stale — lower is better; 0 is ideal.
    pub stale_memory_rate: f64,
    /// Fraction of contradiction-bearing queries where the current fact outranked
    /// the superseded one in the top [`primary_k`], or `None` if no query had a
    /// contradiction annotation.
    pub contradiction_handling_rate: Option<f64>,
}

impl MetricSummary {
    /// Mean Recall at `k`, or `0.0` if that cutoff was not measured.
    pub fn recall(&self, k: usize) -> f64 {
        self.recall_at_k.get(&k).copied().unwrap_or(0.0)
    }

    /// Mean MRR at `k`, or `0.0` if that cutoff was not measured.
    pub fn mrr(&self, k: usize) -> f64 {
        self.mrr_at_k.get(&k).copied().unwrap_or(0.0)
    }

    /// Evaluate this summary against `gate`.
    pub fn gate_verdict(&self, gate: &ReleaseGate) -> GateVerdict {
        let mut reasons = Vec::new();
        let recall = self.recall(gate.k);
        let mrr = self.mrr(gate.k);
        if recall < gate.min_recall {
            reasons.push(format!(
                "recall@{} = {:.3} < {:.2}",
                gate.k, recall, gate.min_recall
            ));
        }
        if mrr < gate.min_mrr {
            reasons.push(format!("mrr@{} = {:.3} < {:.2}", gate.k, mrr, gate.min_mrr));
        }
        if reasons.is_empty() {
            GateVerdict::Pass
        } else {
            GateVerdict::Fail { reasons }
        }
    }
}

/// The full report: every retriever's summary over one golden set, plus the gate
/// used and each retriever's verdict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalReport {
    /// The golden set's name.
    pub golden_set: String,
    /// The cutoffs measured (e.g. `[1, 3, 5, 10]`).
    pub cutoffs: Vec<usize>,
    /// The release gate applied.
    pub gate: ReleaseGate,
    /// One entry per retriever.
    pub retrievers: Vec<RetrieverReport>,
}

/// A single retriever's summary + gate verdict inside an [`EvalReport`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrieverReport {
    /// The aggregated metrics.
    pub summary: MetricSummary,
    /// The gate verdict for this retriever.
    pub verdict: GateVerdict,
}

impl EvalReport {
    /// The report for the retriever named `name`, if present.
    pub fn get(&self, name: &str) -> Option<&RetrieverReport> {
        self.retrievers.iter().find(|r| r.summary.retriever == name)
    }

    /// Serialize to pretty JSON.
    ///
    /// # Errors
    ///
    /// Propagates any `serde_json` serialization error.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// A compact human-readable table for the CLI.
    pub fn to_table(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "golden set: {}  (gate: recall@{}>={:.2}, mrr@{}>={:.2})",
            self.golden_set, self.gate.k, self.gate.min_recall, self.gate.k, self.gate.min_mrr
        );
        let ks: Vec<String> = self.cutoffs.iter().map(|k| format!("R@{k}")).collect();
        let _ = writeln!(
            out,
            "{:<14} {:>24} {:>8} {:>6} {:>6} {:>6} gate",
            "retriever",
            ks.join(" "),
            "MRR@k",
            "cite",
            "stale",
            "contra",
        );
        for r in &self.retrievers {
            let s = &r.summary;
            let recalls: Vec<String> = self
                .cutoffs
                .iter()
                .map(|k| format!("{:>5.3}", s.recall(*k)))
                .collect();
            let cite = s
                .citation_correctness
                .map_or_else(|| "  -  ".to_string(), |v| format!("{v:>5.3}"));
            let contra = s
                .contradiction_handling_rate
                .map_or_else(|| "  -  ".to_string(), |v| format!("{v:>5.3}"));
            let _ = writeln!(
                out,
                "{:<14} {:>24} {:>8.3} {:>6} {:>6.3} {:>6} {}",
                s.retriever,
                recalls.join(" "),
                s.mrr(s.primary_k),
                cite,
                s.stale_memory_rate,
                contra,
                if r.verdict.is_pass() { "PASS" } else { "FAIL" },
            );
        }
        out
    }
}
