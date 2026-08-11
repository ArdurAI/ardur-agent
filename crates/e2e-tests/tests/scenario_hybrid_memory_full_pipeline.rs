//! Scenario — hybrid memory full pipeline.
//!
//! Drives the fused runtime through the real hybrid memory backend against a
//! live Qdrant: chat turn → receipt-chained memory store → hybrid dense+sparse
//! recall → memory context display in the next provider request.
//!
//! `#[ignore]`d because it needs a live Qdrant (CI has none by default); the
//! default suite reports it as `ignored`, never a silent `passed` (#358). Run it
//! explicitly against a Qdrant — the dedicated CI job does exactly this:
//!
//! ```text
//! docker run -p 6333:6333 -p 6334:6334 qdrant/qdrant
//! QDRANT_INTEGRATION_TEST=1 QDRANT_URL=http://localhost:6334 \
//!   cargo test -p ardur-e2e-tests --test scenario_hybrid_memory_full_pipeline \
//!   -- --ignored
//! ```

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use ardur_cost_gate::CostEnvelope;
use ardur_e2e_tests::fixtures::{
    TEST_HOLDER, dev_valid_cap_token, fused_builder, temp_session_root,
};
use ardur_memory::{HolderId, MemoryRuntime, UnixTsMillis};
use ardur_memory_qdrant::{
    Bm25Index, HybridMemoryRetriever, MockEmbedder, QdrantMemoryConfig, QdrantMemoryRuntime,
};
use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, Provider, ProviderError, RateCard, Usage,
};
use ardur_runtime::{CapTokenRef, ChatMessage, ChatRuntime, ProviderId, SessionId, SubmitRequest};
use ardur_session_journals::FileSessionJournal;
use async_trait::async_trait;

const COLLECTION: &str = "ardur_e2e_hybrid_full_pipeline";

fn config() -> QdrantMemoryConfig {
    QdrantMemoryConfig::from_env().with_collection_name(COLLECTION)
}

struct CapturingProvider {
    responses: Mutex<VecDeque<String>>,
    requests: Mutex<Vec<CompletionRequest>>,
    rate_card: RateCard,
}

impl CapturingProvider {
    fn new(responses: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().map(Into::into).collect()),
            requests: Mutex::new(Vec::new()),
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().expect("requests mutex").clone()
    }
}

#[async_trait]
impl Provider for CapturingProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().expect("requests mutex").push(req);
        let content = self
            .responses
            .lock()
            .expect("responses mutex")
            .pop_front()
            .unwrap_or_else(|| "default response".to_string());
        let usage = Usage {
            tokens_in: 12,
            tokens_out: 8,
            cost_cents: Some(1),
        };
        Ok(CompletionResponse {
            content,
            finish_reason: FinishReason::Stop,
            usage,
            cost: self.rate_card.price(usage),
            raw_provider_response: None,
        })
    }

    fn id(&self) -> ProviderId {
        ProviderId("capture".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

fn submit_request(prompt: &str, session_id: SessionId) -> SubmitRequest {
    SubmitRequest {
        messages: vec![ChatMessage::user(prompt)],
        cap_token: CapTokenRef(dev_valid_cap_token()),
        session_id,
        requested_provider: None,
    }
}

#[test]
#[ignore = "requires a live Qdrant; run with `-- --ignored` (see module docs)"]
fn chat_store_recall_and_display_through_real_hybrid_memory() {
    let qdrant = QdrantMemoryRuntime::connect(config()).expect("connect qdrant");
    let bm25 = Bm25Index::new(None).expect("in-memory bm25");
    let hybrid = Arc::new(HybridMemoryRetriever::new(
        qdrant,
        bm25,
        Arc::new(MockEmbedder::new(384)),
    ));
    hybrid.qdrant().delete_collection().ok();
    hybrid.qdrant().init().expect("init qdrant collection");

    let provider = Arc::new(CapturingProvider::new([
        "phoenix rollback runbook requires canary warmup before traffic cutover",
        "second answer uses recalled context",
    ]));
    let root = temp_session_root();
    let session_id = SessionId::new();
    let journal = Arc::new(FileSessionJournal::new(root.path(), session_id).expect("journal"));
    let receipt_log = root.path().join("receipts.jsonl");
    let runtime = fused_builder(provider.clone() as Arc<dyn Provider>)
        .projected_envelope(CostEnvelope {
            tokens_in_max: 1_000,
            tokens_out_max: 1_000,
            cents_max: 10,
            wall_ms_max: 10_000,
            attention_score_max: 10_000,
        })
        .with_memory(hybrid.clone() as Arc<dyn MemoryRuntime + Send + Sync>)
        .with_journal(journal)
        .receipt_log(&receipt_log)
        .build()
        .expect("runtime builds");

    // Keep the outer test synchronous so the Qdrant runtime owned by the hybrid
    // backend is dropped outside an ambient Tokio context. The hybrid backend
    // itself still exercises async Qdrant + BM25 recall through this runtime.
    let async_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test tokio runtime");

    let first = async_rt
        .block_on(runtime.submit(submit_request(
            "Capture the phoenix rollback deployment runbook.",
            session_id,
        )))
        .expect("first chat turn stores memory");

    let stored = hybrid.qdrant().current_as_of(
        &HolderId::from(TEST_HOLDER),
        UnixTsMillis(1_750_000_000_001),
    );
    assert_eq!(stored.len(), 1, "chat turn wrote one durable memory record");
    assert_eq!(
        stored[0].source_receipt_id.map(|id| id.0),
        Some(first.receipt_id.0),
        "memory write is chained to the turn receipt"
    );
    assert!(
        stored[0]
            .payload
            .to_string()
            .contains("phoenix rollback runbook requires canary warmup"),
        "stored memory carries the first provider response: {:?}",
        stored[0].payload
    );

    let direct_hits = async_rt
        .block_on(hybrid.search_for_subject(
            &HolderId::from(TEST_HOLDER),
            "phoenix rollback canary",
            3,
        ))
        .expect("hybrid recall works");
    assert_eq!(
        direct_hits.len(),
        1,
        "hybrid Qdrant+BM25 recall finds the stored turn"
    );
    assert_eq!(direct_hits[0].record_id, stored[0].record_id);

    async_rt
        .block_on(runtime.submit(submit_request(
            "What is the phoenix rollback canary step?",
            session_id,
        )))
        .expect("second chat turn recalls memory");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "provider was called once per chat turn");
    let second_request = &requests[1];
    let displayed_memory = second_request
        .messages
        .iter()
        .find(|msg| msg.content.contains("Relevant memories"))
        .expect("second request displays recalled memory context")
        .content
        .clone();

    assert!(
        displayed_memory.contains("phoenix rollback runbook requires canary warmup"),
        "display includes recalled memory payload: {displayed_memory}"
    );
    assert!(
        displayed_memory.contains(&first.receipt_id.0.to_string()),
        "display includes memory provenance receipt: {displayed_memory}"
    );
    assert!(
        displayed_memory.contains("source=turn")
            && displayed_memory.contains(TEST_HOLDER)
            && displayed_memory.contains("confidence=1.00"),
        "display includes source, scoped subject, and confidence: {displayed_memory}"
    );

    hybrid.qdrant().delete_collection().ok();
}
