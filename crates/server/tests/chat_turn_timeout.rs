//! Regression coverage for issue #359 — the synchronous `POST /chat` turn
//! timeout must not bill for a turn the client was told timed out.
//!
//! Before the fix, the HTTP handler wrapped `submit_chat` in a fixed 30s
//! `tokio::time::timeout`, but the turn itself ran on a detached worker thread
//! that owns the `!Send` fused runtime. When the wait elapsed, the handler
//! returned `504` and dropped its side of the reply channel — yet the worker
//! kept running the turn to completion, minting a receipt and billing cost for
//! a reply the client never received. These tests pin down both halves of the
//! fix: the timeout is configurable, and a timed-out turn is cancelled before
//! it commits a receipt.

mod support;

use std::sync::Arc;
use std::time::Duration;

use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, Provider, ProviderError, ProviderId,
    RateCard, Usage,
};
use ardur_runtime::CostTuple;
use ardur_server::{AppState, Config, build_router, example_registry};
use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;

/// A provider whose `complete` sleeps for `delay` before returning a *billed*
/// reply — long enough to outrun a short HTTP turn timeout.
struct SlowProvider {
    rate_card: RateCard,
    delay: Duration,
}

impl SlowProvider {
    fn new(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            rate_card: RateCard::anthropic_2026_q2_v1(),
            delay,
        })
    }
}

#[async_trait]
impl Provider for SlowProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        tokio::time::sleep(self.delay).await;
        Ok(CompletionResponse {
            content: "slow but complete".to_string(),
            finish_reason: FinishReason::Stop,
            usage: Usage {
                tokens_in: 10,
                tokens_out: 20,
                cost_cents: None,
            },
            cost: CostTuple {
                tokens_in: 10,
                tokens_out: 20,
                cents: 5,
                ..CostTuple::default()
            },
            raw_provider_response: None,
        })
    }

    fn id(&self) -> ProviderId {
        ProviderId("slow".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// Boot state + router over `provider` with a `timeout`-bounded HTTP turn wait.
async fn boot_with_timeout(
    provider: Arc<dyn Provider>,
    timeout: Duration,
) -> (Arc<AppState>, axum::Router) {
    let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
    let mut config: Config = support::test_config(dir, None);
    config.http_turn_timeout = timeout;
    let tools = Arc::new(example_registry("slow", "in-memory"));
    let state = AppState::boot(&config, provider, tools)
        .await
        .expect("AppState boots");
    let router = build_router(Arc::clone(&state));
    (state, router)
}

fn chat_request(message: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/chat")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", support::CHAT_TOKEN))
        .body(Body::from(
            serde_json::json!({ "message": message }).to_string(),
        ))
        .expect("request builds")
}

/// The turn outruns the (tiny) configured timeout: the client must get `504`,
/// and — the crux of #359 — no receipt may be minted for it, because the
/// timed-out turn is cancelled before it commits any receipt/billing side
/// effect. A wait past the provider delay proves the turn was cancelled rather
/// than merely still in flight.
#[tokio::test]
async fn timed_out_turn_returns_504_and_mints_no_receipt() {
    // Provider round takes ~600ms; the HTTP surface only waits 100ms.
    let provider = SlowProvider::new(Duration::from_millis(600));
    let (state, router) = boot_with_timeout(
        Arc::clone(&provider) as Arc<dyn Provider>,
        Duration::from_millis(100),
    )
    .await;

    assert_eq!(state.receipt_count(), 0, "no receipts before the turn");

    let (status, bytes) = support::oneshot(router, chat_request("hello")).await;
    assert_eq!(
        status,
        StatusCode::GATEWAY_TIMEOUT,
        "a turn that outruns the timeout returns 504"
    );
    let json: Value = serde_json::from_slice(&bytes).expect("response body is JSON");
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("timed out"),
        "504 body names the timeout: {json}"
    );

    // Wait well past the provider delay: had the turn *not* been cancelled, the
    // detached worker would have finished the provider round and minted+billed a
    // receipt by now. Cancellation means the count stays at zero.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(
        state.receipt_count(),
        0,
        "a timed-out turn must not mint (or bill) a receipt the client never saw"
    );
}

/// The happy path with a generous timeout: a turn that finishes within the
/// budget returns `200` and *does* mint a receipt. Guards against the fix
/// over-cancelling legitimate turns.
#[tokio::test]
async fn turn_within_timeout_succeeds_and_mints_receipt() {
    let provider = SlowProvider::new(Duration::from_millis(50));
    let (state, router) =
        boot_with_timeout(provider as Arc<dyn Provider>, Duration::from_secs(30)).await;

    let (status, _bytes) = support::oneshot(router, chat_request("hello")).await;
    assert_eq!(status, StatusCode::OK, "a prompt turn returns 200");
    assert_eq!(
        state.receipt_count(),
        1,
        "a completed turn mints exactly one receipt"
    );
}
