//! `boot_hybrid` — [`AppState::boot`] wires the §7.0c `hybrid` memory backend
//! (dense Qdrant + sparse BM25 + an embedder, fused on recall) end-to-end.
//!
//! Gated on `QDRANT_INTEGRATION_TEST=1`: it needs a live Qdrant (the dense half
//! is a real collection) and downloads the BGE-small embedder on first run. The
//! multi-thread flavor matters — boot drives the synchronous Qdrant client
//! (`block_in_place`) from inside the test's ambient runtime, which requires a
//! multi-threaded executor.
//!
//! ```text
//! docker run -p 6334:6334 qdrant/qdrant
//! QDRANT_INTEGRATION_TEST=1 cargo test -p ardur-server --test boot_hybrid
//! ```

mod support;

use ardur_server::MemoryBackend;
use serial_test::serial;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn boot_with_hybrid() {
    if std::env::var("QDRANT_INTEGRATION_TEST").as_deref() != Ok("1") {
        eprintln!(
            "skipping boot_with_hybrid: set QDRANT_INTEGRATION_TEST=1 \
             (needs a live Qdrant + downloads the embedder)"
        );
        return;
    }

    let qdrant_url =
        std::env::var("QDRANT_URL").unwrap_or_else(|_| "http://localhost:6334".to_string());
    let dir = tempfile::tempdir().expect("tempdir");
    let config = ardur_server::Config {
        memory_backend: MemoryBackend::Hybrid,
        qdrant_url: Some(qdrant_url),
        // A dedicated collection so boot's idempotent `init()` does not collide
        // with the durable-Qdrant suites. Set directly on Config rather than by
        // mutating process-global `QDRANT_COLLECTION`.
        qdrant_collection: Some("ardur_boot_hybrid".to_string()),
        ..support::test_config(&dir, None)
    };

    // The whole hybrid substrate wires without panicking: Qdrant connects, the
    // embedder loads, the collection initialises, and the retriever is wrapped
    // behind the `Arc<dyn MemoryRuntime>` seam the fused runtime consumes.
    let state = support::boot_stub(&config).await;
    assert_eq!(state.data_dir(), dir.path());

    // The sparse half is a file-backed BM25 index under `memory/bm25`, so the
    // lexical index is durable like the dense store.
    assert!(
        dir.path().join("memory/bm25").is_dir(),
        "boot lays down the file-backed BM25 index for the hybrid backend"
    );
}
