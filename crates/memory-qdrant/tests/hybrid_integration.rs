//! Live integration tests for [`HybridMemoryRetriever`] — dense + sparse recall
//! over a real Qdrant collection.
//!
//! Two gates:
//! - `QDRANT_INTEGRATION_TEST=1` — needed by every test (the dense half is a real
//!   Qdrant ANN search). Tests that only exercise the wiring use the deterministic
//!   [`MockEmbedder`], so this gate alone runs them.
//! - `EMBEDDINGS_LIVE_TEST=1` — additionally required by the *semantic* tests,
//!   which download and run the real BGE-small model so recall matches on meaning.
//!
//! The multi-thread flavor matters: the hybrid retriever calls the synchronous
//! `QdrantMemoryRuntime` (which bridges its async client with `block_in_place`)
//! from inside this async test, and `block_in_place` requires a multi-threaded
//! runtime.
//!
//! ```text
//! docker run -p 6334:6334 qdrant/qdrant
//! QDRANT_INTEGRATION_TEST=1 cargo test -p ardur-memory-qdrant --test hybrid_integration
//! # plus the semantic tests:
//! QDRANT_INTEGRATION_TEST=1 EMBEDDINGS_LIVE_TEST=1 \
//!   cargo test -p ardur-memory-qdrant --test hybrid_integration
//! ```

use std::sync::Arc;

use ardur_memory::{HolderId, MemoryRecord, MemoryRuntime, RecordKind, UnixTsMillis};
use ardur_memory_qdrant::{
    Bm25Index, Embedder, FastEmbedEmbedder, HybridMemoryRetriever, MockEmbedder,
    QdrantMemoryConfig, QdrantMemoryRuntime,
};

/// `Some(config)` only when the Qdrant gate is set; otherwise the caller returns.
fn qdrant_gate(collection: &str) -> Option<QdrantMemoryConfig> {
    if std::env::var("QDRANT_INTEGRATION_TEST").as_deref() != Ok("1") {
        eprintln!("skipping {collection}: set QDRANT_INTEGRATION_TEST=1 to run");
        return None;
    }
    Some(QdrantMemoryConfig::from_env().with_collection_name(collection))
}

/// Whether the live-embedder gate is set (the semantic tests need it).
fn embeddings_live() -> bool {
    if std::env::var("EMBEDDINGS_LIVE_TEST").as_deref() != Ok("1") {
        eprintln!("skipping semantic assertion: set EMBEDDINGS_LIVE_TEST=1 to run");
        return false;
    }
    true
}

fn fact(subject: &str, predicate: &str, object: &str, t: u64) -> MemoryRecord {
    MemoryRecord::new(
        HolderId::from(subject),
        RecordKind::Fact,
        serde_json::json!({ "predicate": predicate, "object": object }),
        UnixTsMillis(t),
        UnixTsMillis(t),
        None,
        UnixTsMillis(t),
    )
}

/// Build a retriever over `cfg` with the given embedder, on a clean collection.
fn retriever(cfg: QdrantMemoryConfig, embedder: Arc<dyn Embedder>) -> HybridMemoryRetriever {
    let qdrant = QdrantMemoryRuntime::connect(cfg).expect("connect qdrant");
    let bm25 = Bm25Index::new(None).expect("in-memory bm25");
    let hybrid = HybridMemoryRetriever::new(qdrant, bm25, embedder);
    // Init *after* the embedder is attached, so the collection dim matches.
    hybrid.qdrant().delete_collection().ok();
    hybrid.qdrant().init().expect("init collection");
    hybrid
}

fn async_rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test tokio runtime")
}

/// `record` writes to **both** backends: the durable Qdrant store (bi-temporal
/// read finds it) and the BM25 lexical index (a term-only query surfaces it).
#[test]
fn record_writes_to_both() {
    let Some(cfg) = qdrant_gate("ardur_hyb_both") else {
        return;
    };
    let async_rt = async_rt();
    let hybrid = retriever(cfg, Arc::new(MockEmbedder::new(384)));

    let rec = fact("user:both", "prefers", "oolong tea", 1_000);
    let rec_id = rec.record_id;
    async_rt
        .block_on(hybrid.record(rec))
        .expect("record to both");

    // Durable half: the bi-temporal "as-of" read recovers it from Qdrant.
    let durable = hybrid
        .qdrant()
        .current_as_of(&HolderId::from("user:both"), UnixTsMillis(2_000));
    assert_eq!(durable.len(), 1, "the record is in the durable store");
    assert_eq!(durable[0].record_id, rec_id);

    // Lexical half: a bare term query (which the mock embedder cannot match
    // semantically) surfaces it via the BM25 contribution to the fusion.
    let hits = async_rt
        .block_on(hybrid.search("oolong", 5))
        .expect("search");
    assert!(
        hits.iter().any(|r| r.record_id == rec_id),
        "the lexical term hit surfaces the record"
    );

    let empty_hits = async_rt
        .block_on(hybrid.search("   ", 5))
        .expect("empty search");
    assert!(empty_hits.is_empty(), "empty global query returns no hits");
    let empty_scoped_hits = async_rt
        .block_on(hybrid.search_for_subject(&HolderId::from("user:both"), "", 5))
        .expect("empty scoped search");
    assert!(
        empty_scoped_hits.is_empty(),
        "empty scoped query returns no hits"
    );

    hybrid.qdrant().delete_collection().ok();
}

/// `search` returns at most `top_k` records end-to-end.
#[test]
fn search_respects_top_k() {
    let Some(cfg) = qdrant_gate("ardur_hyb_topk") else {
        return;
    };
    let async_rt = async_rt();
    let hybrid = retriever(cfg, Arc::new(MockEmbedder::new(384)));

    for (i, drink) in ["green tea", "black tea", "oolong tea", "herbal tea"]
        .iter()
        .enumerate()
    {
        async_rt
            .block_on(hybrid.record(fact("user:topk", "prefers", drink, 1_000 + i as u64)))
            .expect("record");
    }

    let hits = async_rt.block_on(hybrid.search("tea", 2)).expect("search");
    assert!(
        hits.len() <= 2,
        "top_k caps the result count, got {}",
        hits.len()
    );

    hybrid.qdrant().delete_collection().ok();
}

/// Semantic recall: a query that is lexically *disjoint* from the target but
/// semantically close still surfaces it (the dense half), while the dense half
/// also keeps an unrelated fact away. Gated on the live embedder.
#[test]
fn semantic_hit_gated() {
    let Some(cfg) = qdrant_gate("ardur_hyb_semantic") else {
        return;
    };
    if !embeddings_live() {
        return;
    }
    let async_rt = async_rt();
    let embedder = Arc::new(FastEmbedEmbedder::from_env().expect("load embedder"));
    let hybrid = retriever(cfg, embedder);

    // The target shares no salient words with the query below.
    async_rt
        .block_on(hybrid.record(fact("user:sem", "enjoys", "matcha and oolong", 1_000)))
        .expect("record beverage fact");
    async_rt
        .block_on(hybrid.record(fact(
            "user:sem",
            "schedules",
            "kubernetes pod restarts nightly",
            1_010,
        )))
        .expect("record ops fact");

    // "favorite hot beverage" has no token overlap with either fact, so BM25 is
    // silent and the dense half decides — and it should pick the beverage fact.
    let hits = async_rt
        .block_on(hybrid.search("favorite hot beverage", 1))
        .expect("search");
    assert_eq!(hits.len(), 1, "top_k = 1 returns one record");
    assert_eq!(
        hits[0].payload.get("object").and_then(|v| v.as_str()),
        Some("matcha and oolong"),
        "the semantically-nearest fact is recalled"
    );

    hybrid.qdrant().delete_collection().ok();
}

/// Hybrid beats either retriever alone: the record strong on *both* the lexical
/// and the semantic axis outranks records strong on only one. Gated on the live
/// embedder (so the semantic axis is real).
#[test]
fn hybrid_beats_either() {
    let Some(cfg) = qdrant_gate("ardur_hyb_beats") else {
        return;
    };
    if !embeddings_live() {
        return;
    }
    let async_rt = async_rt();
    let embedder = Arc::new(FastEmbedEmbedder::from_env().expect("load embedder"));
    let hybrid = retriever(cfg, embedder);

    // target: lexical ("green tea") AND semantic (beverage preference) match.
    async_rt
        .block_on(hybrid.record(fact(
            "user:beat",
            "prefers",
            "green tea in the morning",
            1_000,
        )))
        .expect("record target");
    // lexical-only: shares "green" but is about birds, not beverages.
    async_rt
        .block_on(hybrid.record(fact(
            "user:beat",
            "notes",
            "green parrots are loud birds",
            1_010,
        )))
        .expect("record lexical decoy");
    // semantic-only: a beverage, but no lexical overlap with the query.
    async_rt
        .block_on(hybrid.record(fact("user:beat", "drinks", "a strong espresso shot", 1_020)))
        .expect("record semantic decoy");

    let hits = async_rt
        .block_on(hybrid.search("green tea preference", 3))
        .expect("search");
    assert!(!hits.is_empty(), "the fused search returns results");
    assert_eq!(
        hits[0].payload.get("object").and_then(|v| v.as_str()),
        Some("green tea in the morning"),
        "the record strong on both axes ranks first"
    );

    hybrid.qdrant().delete_collection().ok();
}
