//! Shared provider stubs for the fused-runtime end-to-end scenarios: an echoing
//! zero-cost provider and a provider that bills a fixed cost per call (so a
//! finite budget actually depletes across turns).

#![allow(dead_code)] // each scenario uses a different subset.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ardur_provider_runtime::{
    CompletionRequest, CompletionResponse, FinishReason, ModelId, Provider, ProviderError,
    RateCard, Usage,
};
use ardur_runtime::{CostTuple, ProviderId, Role};
use async_trait::async_trait;

/// Echo the most recent user message back.
fn echo_prompt(req: &CompletionRequest) -> String {
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

/// A zero-cost provider that echoes the prompt and counts its calls.
pub struct EchoProvider {
    calls: Arc<AtomicUsize>,
    rate_card: RateCard,
}

impl EchoProvider {
    pub fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for EchoProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            content: echo_prompt(&req),
            finish_reason: FinishReason::Stop,
            usage: Usage {
                tokens_in: 1,
                tokens_out: 1,

                ..Default::default()
            },
            cost: CostTuple::default(),
            raw_provider_response: None,
        })
    }

    fn id(&self) -> ProviderId {
        ProviderId("echo".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// A provider that bills a fixed `cents` per call (other cost dimensions zero),
/// so the cost-gate budget falls by a known amount each finalized turn.
pub struct BillingProvider {
    cents: u64,
    calls: Arc<AtomicUsize>,
    rate_card: RateCard,
}

impl BillingProvider {
    pub fn new(cents: u64) -> Self {
        Self {
            cents,
            calls: Arc::new(AtomicUsize::new(0)),
            rate_card: RateCard::anthropic_2026_q2_v1(),
        }
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for BillingProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            content: echo_prompt(&req),
            finish_reason: FinishReason::Stop,
            usage: Usage {
                tokens_in: 0,
                tokens_out: 0,

                ..Default::default()
            },
            cost: CostTuple {
                tokens_in: 0,
                tokens_out: 0,
                cents: self.cents,
                wall_ms: 0,
                attention_score: 0.0,
            },
            raw_provider_response: None,
        })
    }

    fn id(&self) -> ProviderId {
        ProviderId("billing".to_string())
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn rate_card(&self) -> &RateCard {
        &self.rate_card
    }
}

/// Unused in some scenarios; silences the `ModelId` import there.
pub fn test_model() -> ModelId {
    ModelId::new(ardur_e2e_tests::fixtures::TEST_MODEL)
}
