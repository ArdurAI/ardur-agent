//! Scenario — `scenario_memory_retrieval_eval`.
//!
//! Exercises the Finding-5 retrieval-quality harness (`ardur-memory-eval`)
//! end-to-end from outside the crate: build the BM25, dense, and hybrid
//! retrievers over a labeled golden corpus, run the harness, and assert it
//! produces a well-formed report with the hybrid baseline clearing the V3 §394
//! release gate (recall@5 ≥ 0.70, MRR@5 ≥ 0.55) — the number a future graph-RAG
//! route must beat.
//!
//! Always-on and hermetic: the deterministic `MockEmbedder` (V3 AR-1) stands in
//! for BGE-M3 and Tantivy runs in-RAM, so no Qdrant, model download, or network
//! is required. The real-model baseline is the `ardur-memory-eval --live` path.

use std::sync::Arc;

use ardur_embeddings::MockEmbedder;
use ardur_memory_eval::{
    Bm25Retriever, DenseRetriever, EvalConfig, GoldenSet, HybridRetriever, Retriever, evaluate_all,
};

/// A small engineering-memory corpus whose relevant docs carry distinctive
/// lexical terms, so BM25 (and thus the hybrid) has real signal to measure.
const GOLDEN: &str = r#"{
  "name": "e2e-retrieval-eval",
  "docs": [
    {"id": "d-bind", "kind": "note", "text": "ardur-server binds to port 8080 by default; ARDUR_PORT overrides the listen port."},
    {"id": "d-bind-old", "kind": "note", "stale": true, "text": "Legacy: ardur-server listened on port 9090 before the 8080 change."},
    {"id": "d-rrf", "kind": "markdown", "text": "Hybrid retrieval fuses dense and BM25 results with Reciprocal Rank Fusion at k equals 60."},
    {"id": "d-etxtbsy", "kind": "transcript", "text": "provider-codex retries the subprocess spawn on ETXTBSY Text file busy up to five times."},
    {"id": "d-milli", "kind": "markdown", "text": "attention_score is stored as a milli-attention u64 so cost receipts stay byte-stable."},
    {"id": "d-uuid", "kind": "note", "text": "After the migration a RecordId wraps a time-ordered UUIDv7 minted by the store."},
    {"id": "d-noise", "kind": "markdown", "text": "The CLI renders markdown with comrak and syntect under the night theme."}
  ],
  "queries": [
    {"id": "q-bind", "query": "what port does ardur-server bind to by default", "query_type": "factoid",
     "relevant": {"d-bind": 3}, "expected_citations": ["d-bind"],
     "contradiction": {"current": "d-bind", "superseded": "d-bind-old"}},
    {"id": "q-etxtbsy", "query": "how does provider-codex handle the Text file busy spawn error", "query_type": "multi_hop",
     "relevant": {"d-etxtbsy": 3}},
    {"id": "q-milli", "query": "why is attention_score a milli-attention integer", "query_type": "decision_history",
     "relevant": {"d-milli": 3}},
    {"id": "q-uuid", "query": "what does a RecordId wrap after the migration", "query_type": "temporal",
     "relevant": {"d-uuid": 3}}
  ]
}"#;

#[tokio::test]
async fn memory_retrieval_eval_measures_hybrid_baseline_end_to_end() {
    let golden = GoldenSet::from_json_str(GOLDEN).expect("golden set parses");
    golden.validate().expect("no dangling references");

    // Build the three retrievers over the same corpus (shared, so each is scored
    // standalone and inside the hybrid without re-indexing).
    let bm25: Arc<dyn Retriever> = Arc::new(
        Bm25Retriever::index(&golden.docs)
            .await
            .expect("bm25 index builds"),
    );
    let dense: Arc<dyn Retriever> = Arc::new(
        DenseRetriever::index(MockEmbedder::new(256), &golden.docs)
            .await
            .expect("dense index builds"),
    );
    let hybrid = HybridRetriever::new(dense.clone(), bm25.clone());

    let report = evaluate_all(
        &[&*bm25, &*dense, &hybrid as &dyn Retriever],
        &golden,
        &EvalConfig::default(),
    )
    .await
    .expect("evaluation runs");

    // The report is JSON-serializable (audit surface) and covers all three.
    assert!(report.to_json().is_ok());
    assert_eq!(report.retrievers.len(), 3);

    let bm25_r = report.get("bm25").expect("bm25 present");
    let hybrid_r = report.get("hybrid-rrf").expect("hybrid present");

    // BM25 nails the exact-term corpus and clears the release gate.
    assert!(
        bm25_r.verdict.is_pass(),
        "BM25 should clear the gate: {:?}",
        bm25_r.summary,
    );
    // The hybrid baseline — the number graph-RAG must beat — also clears the gate.
    assert!(
        hybrid_r.verdict.is_pass(),
        "hybrid baseline should clear recall@5>=0.70 / mrr@5>=0.55: {:?}",
        hybrid_r.summary,
    );
    assert!(hybrid_r.summary.recall(5) >= 0.70);

    // Freshness + provenance signals are measured, not vibes: the stale legacy
    // doc is kept out of the top results, and citations land.
    assert!(hybrid_r.summary.stale_memory_rate < 0.25);
    assert_eq!(hybrid_r.summary.citation_correctness, Some(1.0));
    // The contradiction query surfaces the current fact over the superseded one.
    assert_eq!(hybrid_r.summary.contradiction_handling_rate, Some(1.0));
}
