//! ardur-memory-eval — the memory **retrieval-quality** eval harness and
//! hybrid-search baseline.
//!
//! Plan: Finding 5 of `plans/phase-one-plan-advancement-review.md` ("Memory And
//! Vector Retrieval Need An Evaluation Harness"); V3 §294/§394 recall harness.
//! This is the "recall harness" the cycle-3 `MEMORY-V3-EVAL-GATES-SCAFFOLD`
//! design doc explicitly left out of scope — it measures **retrieval** quality
//! (are the right memories retrieved?), distinct from the consolidation-
//! correctness gates (did Dreaming preserve truth?).
//!
//! # Why it exists
//!
//! Without it, memory quality is judged "by vibes instead of recall, precision,
//! freshness, and evidence quality." It computes, over a labeled golden set:
//!
//! - **Recall@K**, **Precision@K**, **nDCG@K**, **MRR@K** — ranking quality;
//! - **citation correctness** — was the source the answer should cite retrieved;
//! - **stale-memory rate** — did the retriever surface invalidated memories;
//! - **contradiction handling** — did it rank the current fact over the stale one.
//!
//! It measures three retrievers through identical code so their numbers are
//! comparable: [`Bm25Retriever`] (lexical, exact strings), [`DenseRetriever`]
//! (embeddings), and the [`HybridRetriever`] that fuses them with Reciprocal Rank
//! Fusion (`k = 60`). **The hybrid number is the baseline any future graph-RAG
//! route must beat** (V3 Q5-a: `recall@5 >= 1.10x` baseline) — graph-RAG is
//! deferred until this baseline is measured.
//!
//! # Hermetic by default
//!
//! The default path uses the deterministic `MockEmbedder` (blessed by V3 AR-1
//! for laying the harness shape) and an in-RAM Tantivy index, so
//! `cargo test --workspace` runs with no network, no model download, and no
//! Qdrant server. The real BGE-M3 baseline runs behind the `live-embed` feature.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod corpus;
pub mod metrics;
pub mod report;
pub mod retriever;
pub mod runner;

pub use corpus::{DocKind, EvalDoc, GoldenQuery, GoldenSet, QueryType};
pub use metrics::ContradictionPair;
pub use report::{EvalReport, GateVerdict, MetricSummary, ReleaseGate, RetrieverReport};
pub use retriever::{
    Bm25Retriever, DenseRetriever, HybridRetriever, PlantedRetriever, RetrieveError, Retriever,
    ScoredDoc,
};
pub use runner::{EvalConfig, evaluate, evaluate_all};
