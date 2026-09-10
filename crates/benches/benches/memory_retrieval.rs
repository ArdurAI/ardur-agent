//! Benchmarks for the hermetic components of hybrid memory retrieval.
//!
//! Production hybrid recall (`HybridMemoryRetriever`) fuses a *dense* half
//! (query embedded, Qdrant ANN search) with a *sparse* half (BM25 lexical) via
//! reciprocal-rank fusion. The dense half needs a running Qdrant service and a
//! downloaded ONNX embedding model, so it is not hermetic and cannot produce a
//! CI-reproducible baseline. What *is* hermetic — and is where the CPU on the
//! recall path actually goes once the network/model latency is set aside — is:
//!
//!  - **`bm25/query`** — the real Tantivy BM25 search (the sparse retriever).
//!  - **`in_process/search`** — `InMemoryMemoryRuntime::search`, the offline
//!    lexical fallback the CLI memory explorer and turn pipeline use without
//!    Qdrant.
//!  - **`embed/mock`** — the deterministic `MockEmbedder`; a stand-in for the
//!    *shape* of the embed step (a per-token pass over the query), not the real
//!    model's cost.
//!  - **`fuse/hybrid`** — reciprocal-rank fusion of a (synthetic) dense list and
//!    the *real* BM25 result list, i.e. the assembly step of `search_filtered`.
//!
//! The corpus is deterministic synthetic text so runs are comparable.

use std::hint::black_box;

use ardur_bm25_index::Bm25Index;
use ardur_embeddings::{Embedder, MockEmbedder};
use ardur_fusion::{DEFAULT_RRF_K, ScoredDoc, reciprocal_rank_fusion};
use ardur_memory::{
    HolderId, InMemoryMemoryRuntime, MemoryRecord, MemoryRuntime, RecordKind, UnixTsMillis,
};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

/// Embedding width used by the default BGE-small model the retriever ships with.
const EMBED_DIM: usize = 384;

/// A small deterministic vocabulary; documents are built by sampling it so the
/// query "systems memory" has a spread of partial matches to score and rank.
const VOCAB: &[&str] = &[
    "systems",
    "memory",
    "retrieval",
    "hybrid",
    "vector",
    "lexical",
    "index",
    "query",
    "fusion",
    "rank",
    "dense",
    "sparse",
    "embedding",
    "cosine",
    "token",
    "budget",
    "receipt",
    "capability",
    "runtime",
    "agent",
];

/// Deterministic document text for doc `i` — six words sampled from `VOCAB` by a
/// cheap LCG so the corpus is fixed and reproducible across runs.
fn doc_text(i: usize) -> String {
    let mut state = (i as u64)
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1);
    let mut words = Vec::with_capacity(6);
    for _ in 0..6 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        words.push(VOCAB[(state >> 33) as usize % VOCAB.len()]);
    }
    words.join(" ")
}

/// A current-thread runtime to drive the async BM25 / embedder surfaces.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
}

/// Build an in-RAM BM25 index of `n` synthetic documents.
fn build_bm25(rt: &tokio::runtime::Runtime, n: usize) -> Bm25Index {
    let mut idx = Bm25Index::new(None).expect("in-ram bm25 index");
    rt.block_on(async {
        for i in 0..n {
            idx.add(format!("doc-{i:05}"), doc_text(i)).await.unwrap();
        }
    });
    idx
}

fn bench_bm25_query(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("memory/bm25_query");
    // Corpus sizes are capped at 1_000: `Bm25Index::add` commits per document
    // (read-your-writes by design), so building the *setup* corpus is O(n)
    // commits — 10_000 would spend minutes in untimed setup for no extra signal
    // about the (measured) query cost.
    for &n in &[100usize, 1_000] {
        let idx = build_bm25(&rt, n);
        // top_k mirrors the retriever's candidate pool for top_k = 10 (= 40).
        group.bench_with_input(BenchmarkId::from_parameter(n), &idx, |b, idx| {
            b.iter(|| {
                rt.block_on(async { idx.query(black_box("systems memory"), 40).await.unwrap() })
            });
        });
    }
    group.finish();
}

/// Build an `InMemoryMemoryRuntime` holding `n` fact records for one subject.
/// Each record's payload splits the synthetic doc text into a `predicate` (first
/// word) and `object` (the rest) — the two fields `search`'s scorer reads.
fn build_in_process(n: usize) -> InMemoryMemoryRuntime {
    let rt = InMemoryMemoryRuntime::new();
    let subject = HolderId::from("user:bench");
    for i in 0..n {
        let text = doc_text(i);
        let (predicate, object) = text.split_once(' ').unwrap_or((text.as_str(), ""));
        rt.record(MemoryRecord::new(
            subject.clone(),
            RecordKind::Fact,
            serde_json::json!({ "predicate": predicate, "object": object }),
            UnixTsMillis(1_000 + i as u64),
            UnixTsMillis(1_000 + i as u64),
            None,
            UnixTsMillis(1_000 + i as u64),
        ))
        .expect("record");
    }
    rt
}

fn bench_in_process_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory/in_process_search");
    for &n in &[100usize, 1_000, 10_000] {
        let rt = build_in_process(n);
        group.bench_with_input(BenchmarkId::from_parameter(n), &rt, |b, rt| {
            b.iter(|| rt.search(black_box("systems memory"), 10).unwrap());
        });
    }
    group.finish();
}

fn bench_mock_embed(c: &mut Criterion) {
    let rt = runtime();
    let embedder = MockEmbedder::new(EMBED_DIM);
    c.bench_function("memory/mock_embed", |b| {
        b.iter(|| {
            rt.block_on(async {
                embedder
                    .embed(black_box(vec!["systems memory retrieval".to_string()]))
                    .await
                    .unwrap()
            })
        });
    });
}

fn bench_fuse_hybrid(c: &mut Criterion) {
    let rt = runtime();
    let idx = build_bm25(&rt, 1_000);
    // The real BM25 candidate list for the query, on the retriever's scale.
    let lexical: Vec<ScoredDoc> = rt
        .block_on(async { idx.query("systems memory", 40).await.unwrap() })
        .into_iter()
        .map(|d| ScoredDoc::new(d.doc_id, f64::from(d.score)))
        .collect();
    // A synthetic dense list overlapping the lexical ids (the dense retriever
    // would surface some of the same docs); ~40 candidates, cosine-ish scores.
    let dense: Vec<ScoredDoc> = (0..40)
        .map(|i| ScoredDoc::new(format!("doc-{i:05}"), 1.0 - (i as f64) / 40.0))
        .collect();
    c.bench_function("memory/fuse_hybrid", |b| {
        b.iter_batched(
            || (dense.clone(), lexical.clone()),
            |(dense, lexical)| reciprocal_rank_fusion(vec![dense, lexical], DEFAULT_RRF_K, 40),
            criterion::BatchSize::SmallInput,
        );
    });
}

criterion_group!(
    benches,
    bench_bm25_query,
    bench_in_process_search,
    bench_mock_embed,
    bench_fuse_hybrid
);
criterion_main!(benches);
