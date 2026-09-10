//! Scenario §2.E — `scheduled_job_execution` (regression guard for issue #347).
//!
//! The unattended-execution path used to be inert: a schedule could be
//! persisted, but no timer ever drove the `ardur-automation` executor, so a due
//! job never ran and never minted a receipt. This scenario proves the bridge is
//! now real end-to-end:
//!
//! 1. A persisted [`AutomationSchedule`] sits in a store, due to fire.
//! 2. A [`ScheduleDriver`] — the timer→executor bridge added for #347 — ticks
//!    once and fires it through the **real** [`FusedRuntime`] pipeline
//!    (cap-token verify → Cedar → cost admission → provider → signed receipt).
//! 3. The fire is delivered to the channel, the store's fire-count is bumped,
//!    and a signed receipt lands on the on-disk chain and verifies under the
//!    publishing JWKS.
//!
//! If any of the three disjoint halves regresses back to "persist but never
//! run", the receipt assertion at the end fails.

use ardur_e2e_tests::fixtures;

use std::sync::Arc;
use std::time::Duration;

use ardur_automation::{
    AutomationAttenuation, AutomationChannel, AutomationDeliveryEvent, AutomationSchedule,
    FusedAutomationRuntime, InMemoryScheduleStore, ProactiveAutomationError,
    ProactiveAutomationLoop, ScheduleDriver, ScheduleStore, ScheduledCapToken,
};
use ardur_cap_token::{CapScope, CapTokenIssuer, HolderId as CapHolderId};
use ardur_cost_gate::{CostEnvelope, CostTuple as GateCostTuple};
use ardur_cron::CronExpression;
use ardur_fused_runtime::{load_persisted_chain, verify_persisted_chain_with_jwks};
use ardur_receipt::Jwks;
use ardur_runtime::{CapTokenRef, SessionId};
use async_trait::async_trait;
use tokio::sync::Mutex;

/// A channel that captures delivered fire events so the test can assert the
/// runtime response reached delivery.
#[derive(Default)]
struct CapturingChannel {
    events: Mutex<Vec<AutomationDeliveryEvent>>,
}

#[async_trait]
impl AutomationChannel for CapturingChannel {
    async fn deliver(
        &self,
        event: AutomationDeliveryEvent,
    ) -> Result<(), ProactiveAutomationError> {
        self.events.lock().await.push(event);
        Ok(())
    }
}

/// Mint an attenuated cap-token the automation loop will accept, scoped to the
/// fixtures' audience/tool/holder so the fused runtime verifies it.
fn attenuated_token() -> ScheduledCapToken {
    let raw = fixtures::dev_cap_issuer()
        .issue(
            CapHolderId(fixtures::TEST_HOLDER.to_string()),
            CapScope {
                audience: fixtures::AUDIENCE.to_string(),
                expires_unix: fixtures::NOW_UNIX + 3_600,
                budget_remaining: 1_000_000,
                tool_allowlist: vec![fixtures::TOOL.to_string()],
            },
        )
        .expect("the fire cap-token issues")
        .to_base64()
        .expect("the fire cap-token serializes");
    ScheduledCapToken::attenuated(
        CapTokenRef(raw),
        vec![AutomationAttenuation {
            rule: format!("restrict_tools:{}", fixtures::TOOL),
            evidence: Some("scheduled unattended fire".to_string()),
        }],
    )
}

#[tokio::test]
async fn due_schedule_fires_through_pipeline_and_mints_receipt() {
    // A file-backed receipt log so we can prove a signed receipt was actually
    // persisted by the fire — not merely returned in memory.
    let session_root = fixtures::temp_session_root();
    let receipt_log = session_root.path().join("chain.jsonl");

    // The real fused runtime over the deterministic stub provider + a generous
    // budget, writing receipts to `receipt_log`.
    let runtime = fixtures::fused_builder(Arc::new(fixtures::stub_provider()))
        .projected_envelope(CostEnvelope {
            cents_max: 1_000,
            ..Default::default()
        })
        .receipt_log(&receipt_log)
        .build()
        .expect("the fused runtime wires");

    // Wire the automation executor over the real runtime.
    let auto_runtime = Arc::new(FusedAutomationRuntime::new(Arc::new(runtime)));
    let store = Arc::new(InMemoryScheduleStore::new());
    let channel = Arc::new(CapturingChannel::default());
    let loop_ = Arc::new(ProactiveAutomationLoop::new(
        auto_runtime,
        store.clone(),
        channel.clone(),
    ));

    // A persisted, always-due schedule.
    let mut schedule = AutomationSchedule::new(
        "nightly-digest",
        CronExpression::every_minute(),
        SessionId::new(),
        attenuated_token(),
        GateCostTuple {
            tokens_in: 1_000_000,
            tokens_out: 1_000_000,
            cents: 100,
            wall_ms: 1_000_000,
            attention_score: 1_000_000,
        },
        "summarize today's activity",
    );
    // Spend against the same holder the runtime provisioned.
    schedule.budget_subject = Some(fixtures::TEST_HOLDER.to_string());
    let id = schedule.id.clone();
    loop_
        .upsert_schedule(schedule)
        .await
        .expect("schedule persists");

    // Nothing has run yet: no receipt on the chain.
    assert!(
        load_persisted_chain(&receipt_log)
            .map(|c| c.is_empty())
            .unwrap_or(true),
        "no receipt exists before the driver ticks"
    );

    // The timer→executor bridge: one bounded tick fires everything due now.
    let driver = ScheduleDriver::new(loop_, Duration::from_millis(10));
    driver.run_bounded(Some(1)).await;

    // 1. The fire was delivered with the runtime's response.
    let events = channel.events.lock().await;
    assert_eq!(
        events.len(),
        1,
        "exactly one due schedule fired and delivered"
    );
    assert_eq!(events[0].schedule_id, id);
    assert_eq!(
        events[0].result.response.content, "[anthropic stub]",
        "the delivered response is the runtime's real completion"
    );

    // 2. The store recorded the successful fire.
    let stored = store.load_all().await.expect("store reloads");
    let fired = stored
        .iter()
        .find(|s| s.id == id)
        .expect("schedule present");
    assert_eq!(fired.fire_count, 1, "the successful fire was persisted");
    assert!(fired.last_fire_at.is_some(), "the fire time was recorded");

    // 3. A signed receipt was minted and chained on disk, and it verifies.
    let chain = load_persisted_chain(&receipt_log).expect("the receipt chain loads");
    assert_eq!(chain.len(), 1, "the fire minted exactly one receipt");
    let jwks = Jwks::from_public_key(&fixtures::dev_receipt_key().public_key());
    verify_persisted_chain_with_jwks(&chain, &jwks)
        .expect("the scheduled fire's receipt chain verifies under the publishing JWKS");

    drop(session_root);
}
