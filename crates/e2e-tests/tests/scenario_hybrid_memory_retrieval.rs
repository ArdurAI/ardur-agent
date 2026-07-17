//! Scenario — `scenario_hybrid_memory_retrieval`.
//!
//! Exercises the hybrid (dense + sparse) recall surface end-to-end against a real
//! Qdrant collection: a [`HybridMemoryRetriever`] records several bi-temporal
//! facts — writing each to **both** the durable Qdrant store (an embedded point)
//! and the BM25 lexical index — then a `search` fuses a vector ANN search with a
//! BM25 query and returns the relevant records.
//!
//! Uses the deterministic [`MockEmbedder`] so the always-on path needs only a live
//! Qdrant (the dense vectors are stable but not semantic); the lexical half of the
//! fusion is what carries recall here. The semantic half is proven with the real
//! model in `ardur-memory-qdrant`'s own `hybrid_integration` suite (gated
//! additionally on `EMBEDDINGS_LIVE_TEST=1`).
//!
//! Gated on `QDRANT_INTEGRATION_TEST=1` (CI has no Qdrant). To run locally:
//!
//! ```text
//! docker run -p 6334:6334 qdrant/qdrant
//! QDRANT_INTEGRATION_TEST=1 \
//!   cargo test -p ardur-e2e-tests --test scenario_hybrid_memory_retrieval
//! ```

use std::sync::Arc;

use ardur_memory::{HolderId, MemoryRecord, MemoryRuntime, RecordKind, UnixTsMillis};
use ardur_memory_qdrant::{
    Bm25Index, HybridMemoryRetriever, MockEmbedder, QdrantMemoryConfig, QdrantMemoryRuntime,
};

const COLLECTION: &str = "ardur_e2e_hybrid_retrieval";
const SUBJECT: &str = "spiffe://ardur/user/e2e-hybrid";

/// The Qdrant config for this scenario, or `None` when the gate var is unset.
fn gate() -> Option<QdrantMemoryConfig> {
    if std::env::var("QDRANT_INTEGRATION_TEST").as_deref() != Ok("1") {
        eprintln!("skipping scenario_hybrid_memory_retrieval: set QDRANT_INTEGRATION_TEST=1");
        return None;
    }
    Some(QdrantMemoryConfig::from_env().with_collection_name(COLLECTION))
}

fn fact(predicate: &str, object: &str, t: u64) -> MemoryRecord {
    MemoryRecord::new(
        HolderId::from(SUBJECT),
        RecordKind::Fact,
        serde_json::json!({ "predicate": predicate, "object": object }),
        UnixTsMillis(t),
        UnixTsMillis(t),
        None,
        UnixTsMillis(t),
    )
}

/// The multi-thread flavor matters: the hybrid retriever calls the synchronous
/// `QdrantMemoryRuntime` (which bridges its async client with `block_in_place`)
/// from inside this async test, and `block_in_place` requires a multi-threaded
/// runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hybrid_records_to_both_and_recalls() {
    let Some(cfg) = gate() else {
        return;
    };

    // Build the retriever on a clean collection. The embedder is attached to the
    // durable runtime, so `init` must run after construction.
    let qdrant = QdrantMemoryRuntime::connect(cfg.clone()).expect("connect qdrant");
    let bm25 = Bm25Index::new(None).expect("in-memory bm25");
    let hybrid = HybridMemoryRetriever::new(qdrant, bm25, Arc::new(MockEmbedder::new(384)));
    hybrid.qdrant().delete_collection().ok();
    hybrid.qdrant().init().expect("init collection");

    // Record three facts through the hybrid surface (each lands in both backends).
    let target = fact("prefers", "oolong tea brewed twice", 1_000);
    let target_id = target.record_id;
    hybrid.record(target).await.expect("record target");
    hybrid
        .record(fact("runs", "kubernetes pods nightly", 1_010))
        .await
        .expect("record decoy 1");
    hybrid
        .record(fact("drives", "a red bicycle to work", 1_020))
        .await
        .expect("record decoy 2");

    // Durable half: every fact is in the Qdrant store under the bi-temporal view.
    let durable = hybrid
        .qdrant()
        .current_as_of(&HolderId::from(SUBJECT), UnixTsMillis(2_000));
    assert_eq!(durable.len(), 3, "all three facts are durably stored");

    // Fused recall: a lexical term unique to the target surfaces it at the top.
    let hits = hybrid.search("oolong", 3).await.expect("search");
    assert!(!hits.is_empty(), "the fused search returns results");
    assert_eq!(
        hits[0].record_id, target_id,
        "the lexically-matching fact is recalled first"
    );

    // top_k is honoured end-to-end.
    let capped = hybrid.search("tea", 1).await.expect("search capped");
    assert!(capped.len() <= 1, "top_k caps the result count");

    hybrid.qdrant().delete_collection().ok();
}
