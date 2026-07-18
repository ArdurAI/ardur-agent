//! Benchmarks for `ardur-fusion` — the rank/score fusion step of hybrid
//! retrieval.
//!
//! Fusion runs on every hybrid recall: the dense (vector) and sparse (BM25)
//! candidate lists are combined into one ranked list before hydration. The
//! `HybridMemoryRetriever` over-fetches `candidate_pool(top_k) = max(top_k * 4,
//! 32)` candidates *per retriever* and fuses those, so the realistic per-list
//! sizes are 32 (the floor, for small `top_k`), 40 (`top_k = 10`), and 400
//! (`top_k = 100`). We bench across that range plus a larger 1024 to see how the
//! algorithm scales.
//!
//! Reciprocal-rank fusion is the strategy the retriever actually uses; the
//! relative-score and distance-score variants are benched too since they share
//! the `finalize` sort and are part of the public surface.
//!
//! The fusion functions take their input lists by value (they consume them), so
//! the per-iteration `clone` is done in `iter_batched`'s untimed setup — the
//! measured routine is the fusion work alone.

use std::hint::black_box;

use ardur_fusion::{
    DEFAULT_RRF_K, ScoredDoc, distance_score_fusion, reciprocal_rank_fusion, relative_score_fusion,
};
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};

/// Build two candidate lists of `n` docs each that mirror a real hybrid pool:
/// the dense and sparse retrievers agree on roughly half their ids (the overlap
/// RRF exists to reward) and disagree on the rest. Scores are deterministic and
/// on the two retrievers' natural scales (cosine-ish `[0, 1]` for dense, BM25
/// `[0, ~30)` for sparse).
fn hybrid_pair(n: usize) -> Vec<Vec<ScoredDoc>> {
    let dense: Vec<ScoredDoc> = (0..n)
        .map(|i| ScoredDoc::new(format!("doc-{i:05}"), 1.0 - (i as f64) / (n as f64)))
        .collect();
    // The sparse list shares every other id with the dense one, and introduces
    // fresh ids for the rest — so ~50% overlap, the interesting fusion case.
    let sparse: Vec<ScoredDoc> = (0..n)
        .map(|i| {
            let id = if i % 2 == 0 {
                format!("doc-{i:05}")
            } else {
                format!("sparse-{i:05}")
            };
            ScoredDoc::new(id, 30.0 * (1.0 - (i as f64) / (n as f64)))
        })
        .collect();
    vec![dense, sparse]
}

fn bench_rrf(c: &mut Criterion) {
    let mut group = c.benchmark_group("fusion/reciprocal_rank");
    for &n in &[32usize, 40, 400, 1024] {
        let lists = hybrid_pair(n);
        group.bench_with_input(BenchmarkId::from_parameter(n), &lists, |b, lists| {
            b.iter_batched(
                || lists.clone(),
                |lists| reciprocal_rank_fusion(lists, DEFAULT_RRF_K, black_box(n)),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_relative(c: &mut Criterion) {
    let mut group = c.benchmark_group("fusion/relative_score");
    for &n in &[32usize, 400] {
        let lists = hybrid_pair(n);
        group.bench_with_input(BenchmarkId::from_parameter(n), &lists, |b, lists| {
            b.iter_batched(
                || lists.clone(),
                |lists| relative_score_fusion(lists, None, black_box(n)),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_distance(c: &mut Criterion) {
    let mut group = c.benchmark_group("fusion/distance_score");
    for &n in &[32usize, 400] {
        let lists = hybrid_pair(n);
        group.bench_with_input(BenchmarkId::from_parameter(n), &lists, |b, lists| {
            b.iter_batched(
                || lists.clone(),
                |lists| distance_score_fusion(lists, black_box(n)),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_rrf, bench_relative, bench_distance);
criterion_main!(benches);
