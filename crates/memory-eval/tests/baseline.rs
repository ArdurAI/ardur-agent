//! The hybrid-search baseline measurement (Finding 5): score BM25, dense, and
//! hybrid retrievers over the golden corpus and assert the harness produces a
//! well-formed, discriminating report.
//!
//! This is the "baseline is measured" deliverable that GATES graph-RAG — the
//! printed numbers are the bar a graph route must beat (V3 Q5-a: recall@5 >=
//! 1.10x baseline). It runs hermetically: the deterministic `MockEmbedder` (V3
//! AR-1) stands in for BGE-M3 and Tantivy runs in-RAM, so there is no network,
//! no model download, and no Qdrant server. The real-model baseline numbers come
//! from the `--live` CLI path (feature `live-embed`).

use std::collections::HashMap;
use std::sync::Arc;

use ardur_embeddings::MockEmbedder;
use ardur_memory_eval::{
    Bm25Retriever, DenseRetriever, EvalConfig, GoldenSet, HybridRetriever, PlantedRetriever,
    Retriever, evaluate_all,
};

fn golden() -> GoldenSet {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/golden.json");
    let set = GoldenSet::from_json_file(path).expect("golden fixture loads");
    set.validate()
        .expect("golden fixture has no dangling references");
    set
}

#[tokio::test]
async fn hybrid_baseline_is_measured_and_well_formed() {
    let golden = golden();
    assert_eq!(
        golden.queries.len(),
        8,
        "the golden set covers the taxonomy"
    );

    let bm25: Arc<dyn Retriever> = Arc::new(Bm25Retriever::index(&golden.docs).await.unwrap());
    let dense: Arc<dyn Retriever> = Arc::new(
        DenseRetriever::index(MockEmbedder::new(384), &golden.docs)
            .await
            .unwrap(),
    );
    let hybrid = HybridRetriever::new(dense.clone(), bm25.clone());

    let report = evaluate_all(
        &[&*bm25, &*dense, &hybrid as &dyn Retriever],
        &golden,
        &EvalConfig::default(),
    )
    .await
    .unwrap();

    // Print the measured baseline so it lands in CI logs (`--nocapture`).
    println!("\n{}", report.to_table());

    // --- Structural correctness ---
    assert_eq!(report.retrievers.len(), 3);
    for r in &report.retrievers {
        let s = &r.summary;
        assert_eq!(s.queries_scored, 8, "every query has ground truth");
        for (&k, &v) in &s.recall_at_k {
            assert!((0.0..=1.0).contains(&v), "recall@{k} = {v} out of range");
        }
        for (&k, &v) in &s.ndcg_at_k {
            assert!((0.0..=1.0).contains(&v), "ndcg@{k} = {v} out of range");
        }
        for (&k, &v) in &s.mrr_at_k {
            assert!((0.0..=1.0).contains(&v), "mrr@{k} = {v} out of range");
        }
        assert!((0.0..=1.0).contains(&s.stale_memory_rate));
        if let Some(c) = s.citation_correctness {
            assert!((0.0..=1.0).contains(&c));
        }
    }

    let bm25_s = &report.get("bm25").unwrap().summary;
    let dense_s = &report.get("dense").unwrap().summary;
    let hybrid_s = &report.get("hybrid-rrf").unwrap().summary;

    // --- The harness discriminates retriever quality ---
    // Real lexical BM25 must beat the non-semantic MockEmbedder dense retriever on
    // this lexical corpus; if it didn't, the metrics would be measuring noise.
    assert!(
        bm25_s.recall(5) > dense_s.recall(5),
        "BM25 recall@5 ({:.3}) should beat non-semantic dense ({:.3})",
        bm25_s.recall(5),
        dense_s.recall(5),
    );
    assert!(
        bm25_s.recall(5) >= 0.5,
        "BM25 should find most exact-term relevant docs, got recall@5 {:.3}",
        bm25_s.recall(5),
    );

    // --- RRF robustness: fusing a noisy retriever must not tank the hybrid ---
    assert!(
        hybrid_s.recall(5) >= dense_s.recall(5),
        "hybrid recall@5 ({:.3}) must not fall below the dense floor ({:.3})",
        hybrid_s.recall(5),
        dense_s.recall(5),
    );
    assert!(hybrid_s.recall(5) > 0.0);

    // Stale-memory rate is a real signal: one doc (d-port-old) is stale, and a
    // decent retriever keeps it out of most top-5s.
    assert!(
        bm25_s.stale_memory_rate < 0.25,
        "BM25 surfaced too many stale docs: {:.3}",
        bm25_s.stale_memory_rate,
    );
}

/// A designed scenario proving the harness credits the hybrid for beating *both*
/// single retrievers when each is strong on a different query — the property that
/// justifies hybrid as the baseline. Uses planted retrievers so the ranking is
/// exact and the assertion is not embedder-dependent.
#[tokio::test]
async fn hybrid_beats_both_singles_when_each_wins_different_queries() {
    // Corpus: qa's answer is "a", qb's answer is "b".
    let golden = GoldenSet::from_json_str(
        r#"{
          "name":"split","docs":[
            {"id":"a","text":"x","kind":"note"},
            {"id":"b","text":"y","kind":"note"},
            {"id":"z","text":"z","kind":"note"}
          ],
          "queries":[
            {"id":"qa","query":"qa","query_type":"factoid","relevant":{"a":3}},
            {"id":"qb","query":"qb","query_type":"factoid","relevant":{"b":3}}
          ]
        }"#,
    )
    .unwrap();

    // Dense nails qa (ranks a first) but whiffs qb (b buried past top-2).
    let mut dense_plan: HashMap<String, Vec<String>> = HashMap::new();
    dense_plan.insert("qa".into(), vec!["a".into(), "z".into(), "b".into()]);
    dense_plan.insert("qb".into(), vec!["z".into(), "a".into(), "b".into()]);
    // BM25 nails qb (ranks b first) but whiffs qa (a buried past top-2).
    let mut bm25_plan: HashMap<String, Vec<String>> = HashMap::new();
    bm25_plan.insert("qa".into(), vec!["z".into(), "b".into(), "a".into()]);
    bm25_plan.insert("qb".into(), vec!["b".into(), "z".into(), "a".into()]);

    let dense: Arc<dyn Retriever> = Arc::new(PlantedRetriever::new("dense", dense_plan));
    let bm25: Arc<dyn Retriever> = Arc::new(PlantedRetriever::new("bm25", bm25_plan));
    let hybrid = HybridRetriever::new(dense.clone(), bm25.clone());

    // Measure at k=2, where RRF has room to fuse each retriever's one correct
    // answer into the top-2 for *both* queries.
    let cfg = EvalConfig {
        cutoffs: vec![2],
        primary_k: 2,
        ..EvalConfig::default()
    };
    let report = evaluate_all(&[&*dense, &*bm25, &hybrid as &dyn Retriever], &golden, &cfg)
        .await
        .unwrap();

    let dense_r = report.get("dense").unwrap().summary.recall(2);
    let bm25_r = report.get("bm25").unwrap().summary.recall(2);
    let hybrid_r = report.get("hybrid-rrf").unwrap().summary.recall(2);

    // Each single gets exactly one of the two queries into its top-2 → 0.5.
    assert!((dense_r - 0.5).abs() < 1e-9, "dense recall@2 = {dense_r}");
    assert!((bm25_r - 0.5).abs() < 1e-9, "bm25 recall@2 = {bm25_r}");
    // Hybrid fuses both right answers into the top-2 → recall@2 = 1.0 > both.
    assert!(
        hybrid_r > dense_r && hybrid_r > bm25_r,
        "hybrid recall@2 ({hybrid_r}) should beat both singles ({dense_r}, {bm25_r})"
    );
    assert!((hybrid_r - 1.0).abs() < 1e-9);
}
