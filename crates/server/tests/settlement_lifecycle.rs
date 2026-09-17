//! Product worker ownership: a real SSE body drop must be drained before reuse.
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, Provider, ProviderError, ProviderId,
    ProviderStream, RateCard, StreamEvent, Usage,
};
use ardur_runtime::{CostTuple, SessionId};
use ardur_server::{AppState, build_router, example_registry};
use ardur_session_journals::JournalEntry;
use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use futures::StreamExt;
use serde_json::json;
use tower::ServiceExt;

struct PaidProvider {
    calls: AtomicUsize,
    rate: RateCard,
    saturated: Option<Arc<tokio::sync::Notify>>,
}
#[async_trait]
impl Provider for PaidProvider {
    async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            content: "healthy".into(),
            finish_reason: FinishReason::Stop,
            usage: Usage {
                cost_cents: Some(9),
                ..Usage::default()
            },
            cost: CostTuple {
                cents: 9,
                ..CostTuple::default()
            },
            raw_provider_response: None,
        })
    }
    async fn stream(&self, _: CompletionRequest) -> Result<ProviderStream, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // The content is a deterministic barrier: the runtime has durably
        // observed the preceding Usage before the real HTTP body exposes it.
        let head = futures::stream::iter([
            Ok(StreamEvent::Usage(Usage {
                cost_cents: Some(9),
                ..Usage::default()
            })),
            Ok(StreamEvent::ContentDelta("usage-observed".into())),
        ]);
        if let Some(saturated) = self.saturated.clone() {
            return Ok(Box::pin(head.chain(futures::stream::unfold(
                (0, saturated),
                |(i, signal)| async move {
                    if i == 16 {
                        signal.notify_one();
                    }
                    Some((
                        Ok(StreamEvent::ContentDelta("more".into())),
                        (i + 1, signal),
                    ))
                },
            ))));
        }
        Ok(Box::pin(head.chain(futures::stream::pending())))
    }
    fn id(&self) -> ProviderId {
        ProviderId("local-paid".into())
    }
    fn supports_streaming(&self) -> bool {
        true
    }
    fn rate_card(&self) -> &RateCard {
        &self.rate
    }
}
fn request(session: SessionId, stream: bool) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/chat")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", support::CHAT_TOKEN))
        .body(Body::from(
            json!({"message":"local", "session_id":session.0, "stream":stream}).to_string(),
        ))
        .unwrap()
}

#[tokio::test]
async fn abandoned_http_usage_drains_before_healthy_continuation() {
    tokio::time::timeout(Duration::from_secs(20), async {
        let dir = tempfile::tempdir().unwrap();
        let config = support::test_config(&dir, None);
        let provider = Arc::new(PaidProvider {
            calls: AtomicUsize::new(0),
            saturated: None,
            rate: RateCard { version_id:"local".into(), cents_per_1k_input:0.0, cents_per_1k_output:0.0, cents_per_request:0.0 },
        });
        let state = AppState::boot(&config, provider.clone(), Arc::new(example_registry("local", "in-memory")),
            ardur_fused_runtime::SharedDenyList::new()).await.unwrap();
        let router = build_router(state.clone());
        for index in 0..5 {
            let session = SessionId::new();
            assert_ne!(session, *state.journal().session_id());
            let response = router.clone().oneshot(request(session, true)).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let mut body = response.into_body().into_data_stream();
            while let Some(frame) = body.next().await {
                if String::from_utf8_lossy(&frame.unwrap()).contains("usage-observed") { break; }
            }
            // Queued behind the blocked first stream; dropping this body must
            // not dispatch a provider at all.
            let queued = router.clone().oneshot(request(SessionId::new(), true)).await.unwrap();
            drop(queued);
            drop(body);
            // FIFO completion is an explicit marker for worker-owned cleanup,
            // not a sleep or a test calling drain on the worker's behalf.
            let healthy = router.clone().oneshot(request(SessionId::new(), false)).await.unwrap();
            assert_eq!(healthy.status(), StatusCode::OK);
            let value: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(healthy.into_body(), usize::MAX).await.unwrap()).unwrap();
            assert_eq!(value["cost_usd"], 0.09, "{value}");
            let entries = state.journal().replay(*state.journal().session_id()).await.unwrap();
            assert_eq!(entries.iter().filter(|e| matches!(e, JournalEntry::OperatorExpense { session_id, provider_cost, .. } if *session_id == session && provider_cost.cents == 9)).count(), 1,
                "owning-body cancellation must project exactly one known operator expense");
            assert_eq!(provider.calls.load(Ordering::SeqCst), (index + 1) * 2, "queued abandoned request must not dispatch");
            assert_eq!(state.receipt_count(), index + 1, "no false receipt for abandonment");
            assert_eq!(entries.iter().filter(|e| matches!(e, JournalEntry::AssistantMessage { .. })).count(), index + 1);
        }
        state.finish_shutdown().await.unwrap();
        assert!(!state.worker_alive());
        let stats = state.receipt_stats();
        assert!(stats.chain_verified);
        assert_eq!(stats.cost_cents_sum, 45, "receipt cost is not added again from journal");
        let supervisor = state.settlement_supervisor();
        assert!(supervisor.status().turns.is_empty());
        let snapshots = supervisor.durable_snapshots().unwrap();
        assert_eq!(snapshots.iter().filter(|s| s.turn().terminal == ardur_session_journals::settlement::TurnTerminal::Cancelled).count(), 5);
        for snapshot in snapshots.iter().filter(|s| s.turn().terminal == ardur_session_journals::settlement::TurnTerminal::Cancelled) {
            let ardur_session_journals::settlement::SettlementPhase::Settled { application, .. } = &snapshot.turn().rounds[0].phase else { panic!("not durable settled cancellation") };
            assert_eq!(application.applied_debit.cents, 0);
            assert_eq!(snapshot.turn().rounds[0].known_incurred.cents, 9);
        }
        let before = state.journal().replay(*state.journal().session_id()).await.unwrap();
        drop(supervisor);
        drop(router);
        drop(state);
        let reopened = AppState::boot(&config, provider, Arc::new(example_registry("local", "in-memory")), ardur_fused_runtime::SharedDenyList::new()).await.unwrap();
        assert!(reopened.settlement_supervisor().status().boot_problem.is_none());
        assert_eq!(reopened.journal().replay(*reopened.journal().session_id()).await.unwrap(), before, "reopen must not synthesize assistant text or accounting");
        assert_eq!(reopened.receipt_stats().cost_cents_sum, 45);
        reopened.finish_shutdown().await.unwrap();
    }).await.expect("HTTP worker makes bounded progress with local provider");
}

#[tokio::test]
async fn abandoned_http_is_projected_before_worker_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let config = support::test_config(&dir, None);
    let saturated = Arc::new(tokio::sync::Notify::new());
    let provider = Arc::new(PaidProvider {
        calls: AtomicUsize::new(0),
        saturated: Some(saturated.clone()),
        rate: RateCard {
            version_id: "local".into(),
            cents_per_1k_input: 0.0,
            cents_per_1k_output: 0.0,
            cents_per_request: 0.0,
        },
    });
    let state = AppState::boot(
        &config,
        provider,
        Arc::new(example_registry("local", "in-memory")),
        ardur_fused_runtime::SharedDenyList::new(),
    )
    .await
    .unwrap();
    let session = SessionId::new();
    let response = build_router(state.clone())
        .oneshot(request(session, true))
        .await
        .unwrap();
    let mut body = response.into_body().into_data_stream();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(frame) = body.next().await {
            if String::from_utf8_lossy(&frame.unwrap()).contains("usage-observed") {
                return;
            }
        }
        panic!("known usage barrier missing");
    })
    .await
    .unwrap();
    // The seventeenth post-marker delta was polled; the actual 16-slot
    // forwarder cannot send it until capacity returns or the body is dropped.
    tokio::time::timeout(Duration::from_secs(5), saturated.notified())
        .await
        .unwrap();
    drop(body);
    state.shutdown();
    let entries = state
        .journal()
        .replay(*state.journal().session_id())
        .await
        .unwrap();
    assert_eq!(entries.iter().filter(|e| matches!(e, JournalEntry::OperatorExpense { session_id, provider_cost, .. } if *session_id == session && provider_cost.cents == 9)).count(), 1,
        "worker shutdown must project cancellation without a subsequent turn");
    state.finish_shutdown().await.unwrap();
    assert!(!state.worker_alive());
    assert_eq!(state.receipt_count(), 0);
    assert!(
        !entries
            .iter()
            .any(|e| matches!(e, JournalEntry::AssistantMessage { .. }))
    );
}
