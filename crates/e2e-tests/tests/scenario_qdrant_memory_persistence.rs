//! Scenario — `scenario_qdrant_memory_persistence`.
//!
//! Proves the durable §7.0 Phase 2 memory backend (`ardur-memory-qdrant`)
//! survives a process restart on the *fused* call path: a full chat turn runs
//! through [`FusedRuntime`] with a Qdrant memory sink (stage 9 records the turn
//! as a bi-temporal `Observation`); we then drop the entire runtime + backend
//! (its Qdrant client and Tokio bridge — a simulated restart), reconnect a
//! brand-new backend to the same collection, and read the fact back. The §7.0
//! Phase 1 in-process store would lose the fact here; the Qdrant store recovers
//! it.
//!
//! Gated on `QDRANT_INTEGRATION_TEST=1` (CI has no Qdrant). To run locally:
//!
//! ```text
//! docker run -p 6334:6334 qdrant/qdrant
//! QDRANT_INTEGRATION_TEST=1 \
//!   cargo test -p ardur-e2e-tests --test scenario_qdrant_memory_persistence
//! ```

use std::sync::Arc;

use ardur_e2e_tests::fixtures::{self, NOW_MS, TEST_HOLDER};
use ardur_memory::{HolderId, MemoryRuntime, RecordKind, UnixTsMillis};
use ardur_memory_qdrant::{QdrantMemoryConfig, QdrantMemoryRuntime};
use ardur_provider_runtime::Provider;
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, SessionId, SubmitRequest};

const PROMPT: &str = "remember this across a restart";
const COLLECTION: &str = "ardur_e2e_qdrant_persistence";

/// The Qdrant config for this scenario, or `None` when the gate var is unset.
fn gate() -> Option<QdrantMemoryConfig> {
    if std::env::var("QDRANT_INTEGRATION_TEST").as_deref() != Ok("1") {
        eprintln!("skipping scenario_qdrant_memory_persistence: set QDRANT_INTEGRATION_TEST=1");
        return None;
    }
    Some(QdrantMemoryConfig::from_env().with_collection_name(COLLECTION))
}

/// The multi-thread flavor matters: the fused turn calls the synchronous
/// `MemoryRuntime::record` from inside this runtime, and the Qdrant backend
/// bridges it with `block_in_place`, which requires a multi-threaded runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fused_turn_memory_survives_restart() {
    let Some(cfg) = gate() else {
        return;
    };

    let subject = HolderId::from(TEST_HOLDER);

    // ---- First "process": run a fused turn that writes to the durable store.
    {
        // Clean slate, then connect the backend the fused runtime will record to.
        let qdrant = QdrantMemoryRuntime::connect(cfg.clone()).expect("connect qdrant");
        qdrant.delete_collection().ok();
        qdrant.init().expect("init collection");

        let memory: Arc<dyn MemoryRuntime + Send + Sync> = Arc::new(qdrant);
        let provider: Arc<dyn Provider> = Arc::new(fixtures::stub_provider());
        let receipt_log = fixtures::temp_session_root();

        let runtime = fixtures::fused_builder(provider)
            .with_memory(memory.clone())
            .receipt_log(receipt_log.path().join("chain.jsonl"))
            .build()
            .expect("the fused runtime builds with the qdrant memory sink");

        let outcome = runtime
            .submit(SubmitRequest {
                messages: vec![ChatMessage::user(PROMPT)],
                cap_token: CapTokenRef(fixtures::dev_valid_cap_token()),
                session_id: SessionId::new(),
                requested_provider: None,
            })
            .await
            .expect("the fused turn completes and records to qdrant memory");
        assert_eq!(
            outcome.response.content, "[anthropic stub]",
            "the stub provider's deterministic completion came back"
        );

        // Drop everything that touches Qdrant: the runtime (which holds one Arc)
        // and our own Arc. After this block nothing in-process retains the fact.
        drop(runtime);
        drop(memory);
    }

    // ---- Second "process": a fresh backend over the same collection recovers
    //      the turn the first process recorded.
    {
        let recovered = QdrantMemoryRuntime::connect_and_init(cfg.clone()).expect("reconnect");
        // The fused store records the turn at the runtime's manual-clock `now`
        // (`NOW_MS`), open-ended, so it is live as-of any later instant.
        let facts = recovered.current_as_of(&subject, UnixTsMillis(NOW_MS + 1));
        assert_eq!(
            facts.len(),
            1,
            "exactly one turn fact survived the restart for {TEST_HOLDER}"
        );
        let fact = &facts[0];
        assert!(
            matches!(fact.kind, RecordKind::Observation),
            "the fused runtime records a turn as an Observation"
        );
        assert_eq!(
            fact.payload.get("response").and_then(|v| v.as_str()),
            Some("[anthropic stub]"),
            "the recovered fact carries the turn's response"
        );

        recovered.delete_collection().ok();
    }
}
