//! Timer → executor bridge for proactive automation schedules (issue #347).
//!
//! [`ProactiveAutomationLoop::fire_due`](crate::proactive::ProactiveAutomationLoop::fire_due)
//! already executes due schedules end-to-end — validate → fused-runtime submit →
//! channel delivery → fire-count persistence — but nothing in the substrate ever
//! drove it on a clock. Persisted schedules were therefore inert: created,
//! stored, and never run. [`ScheduleDriver`] is the missing bridge. It owns a
//! loop and ticks it on a fixed interval, firing every due schedule each tick.
//!
//! The driver deliberately mirrors the shape of
//! [`ardur_cron::CronScheduler`](https://docs.rs) — `new` / `start` / `stop` /
//! `is_running` — but where that type only marks job lifecycle, this one calls
//! the real executor. Callers that want a bounded, inline run (a CLI
//! `schedule run --max-ticks`, or a test) use [`ScheduleDriver::run_bounded`];
//! callers that want an always-on background driver (a server boot) use
//! [`ScheduleDriver::start`].

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::proactive::{
    AutomationChannel, AutomationRuntime, FireReport, ProactiveAutomationLoop, ScheduleStore,
};

/// Drives a [`ProactiveAutomationLoop`] on a fixed tick interval, firing every
/// due schedule each tick.
pub struct ScheduleDriver<R, S, C> {
    loop_: Arc<ProactiveAutomationLoop<R, S, C>>,
    tick_interval: Duration,
    handle: Arc<RwLock<Option<JoinHandle<()>>>>,
}

impl<R, S, C> ScheduleDriver<R, S, C>
where
    R: AutomationRuntime,
    S: ScheduleStore,
    C: AutomationChannel,
{
    /// Wrap an automation loop and tick it every `tick_interval`.
    #[must_use]
    pub fn new(loop_: Arc<ProactiveAutomationLoop<R, S, C>>, tick_interval: Duration) -> Self {
        Self {
            loop_,
            tick_interval,
            handle: Arc::new(RwLock::new(None)),
        }
    }

    /// Fire every schedule due at `now` exactly once. A store-read failure is
    /// logged and swallowed so one bad tick never tears the driver down; the
    /// returned reports cover the schedules that were actually considered.
    pub async fn tick_once(&self, now: chrono::DateTime<Utc>) -> Vec<FireReport> {
        match self.loop_.fire_due(now).await {
            Ok(reports) => {
                for report in &reports {
                    if report.delivered {
                        info!(schedule = %report.schedule_id.0, "fired due schedule");
                    } else if let Some(err) = &report.error {
                        warn!(schedule = %report.schedule_id.0, error = %err, "schedule fire failed");
                    }
                }
                reports
            }
            Err(err) => {
                warn!(error = %err, "failed to load due schedules for this tick");
                Vec::new()
            }
        }
    }

    /// Run the driver inline until `max_ticks` ticks have elapsed, or forever
    /// when `max_ticks` is `None`.
    ///
    /// The first tick fires immediately (matching [`tokio::time::interval`]
    /// semantics), so `run_bounded(Some(1))` is "fire everything due right now,
    /// once, then return" — the shape a one-shot CLI drive and the E2E tests
    /// rely on.
    pub async fn run_bounded(&self, max_ticks: Option<usize>) {
        let mut ticker = tokio::time::interval(self.tick_interval);
        let mut fired = 0usize;
        loop {
            ticker.tick().await;
            self.tick_once(Utc::now()).await;
            fired = fired.saturating_add(1);
            if max_ticks.is_some_and(|max| fired >= max) {
                break;
            }
        }
    }

    /// Spawn a background task that drives the loop forever on the tick
    /// interval. Idempotent-guarded: a second `start` while already running is a
    /// no-op that returns `false`.
    pub async fn start(&self) -> bool {
        let mut handle = self.handle.write().await;
        if handle.is_some() {
            return false;
        }
        let loop_ = Arc::clone(&self.loop_);
        let interval = self.tick_interval;
        let h = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                let now = Utc::now();
                match loop_.fire_due(now).await {
                    Ok(reports) => {
                        for report in &reports {
                            if report.delivered {
                                info!(schedule = %report.schedule_id.0, "fired due schedule");
                            } else if let Some(err) = &report.error {
                                warn!(schedule = %report.schedule_id.0, error = %err, "schedule fire failed");
                            }
                        }
                    }
                    Err(err) => {
                        warn!(error = %err, "failed to load due schedules for this tick");
                    }
                }
            }
        });
        *handle = Some(h);
        info!("automation schedule driver started");
        true
    }

    /// Abort the background driver. Returns `false` when it was not running.
    pub async fn stop(&self) -> bool {
        let mut handle = self.handle.write().await;
        if let Some(h) = handle.take() {
            h.abort();
            info!("automation schedule driver stopped");
            true
        } else {
            false
        }
    }

    /// Whether a background driver spawned by [`start`](Self::start) is live.
    pub async fn is_running(&self) -> bool {
        self.handle.read().await.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proactive::{
        AutomationAttenuation, AutomationDeliveryEvent, AutomationSchedule, InMemoryScheduleStore,
        ProactiveAutomationError, ScheduledCapToken,
    };

    use ardur_cost_gate::CostTuple as GateCostTuple;
    use ardur_cron::CronExpression;
    use ardur_fused_runtime::PerRequestProvisioning;
    use ardur_runtime::{
        CapTokenRef, ChatMessage, CostTuple as RuntimeCostTuple, ReceiptId, RuntimeError,
        SessionId, SubmitRequest, SubmitResult,
    };
    use async_trait::async_trait;
    use tokio::sync::Mutex;

    #[derive(Debug, Default)]
    struct RecordingRuntime {
        submits: Mutex<usize>,
    }

    #[async_trait]
    impl AutomationRuntime for RecordingRuntime {
        async fn submit(
            &self,
            _req: SubmitRequest,
            _provisioning: PerRequestProvisioning,
        ) -> Result<SubmitResult, RuntimeError> {
            *self.submits.lock().await += 1;
            Ok(SubmitResult {
                receipt_id: ReceiptId::new(),
                response: ChatMessage::assistant("driven"),
                cost: RuntimeCostTuple::default(),
            })
        }
    }

    #[derive(Debug, Default)]
    struct RecordingChannel {
        delivered: Mutex<usize>,
    }

    #[async_trait]
    impl AutomationChannel for RecordingChannel {
        async fn deliver(
            &self,
            _event: AutomationDeliveryEvent,
        ) -> Result<(), ProactiveAutomationError> {
            *self.delivered.lock().await += 1;
            Ok(())
        }
    }

    fn token() -> ScheduledCapToken {
        ScheduledCapToken::attenuated(
            CapTokenRef("attenuated".to_string()),
            vec![AutomationAttenuation {
                rule: "restrict_tools:chat.submit".to_string(),
                evidence: None,
            }],
        )
    }

    fn budget() -> GateCostTuple {
        GateCostTuple {
            tokens_in: 100,
            tokens_out: 100,
            cents: 10,
            wall_ms: 1_000,
            attention_score: 1,
        }
    }

    /// The timer bridge actually executes a persisted due schedule: one bounded
    /// tick submits through the runtime, delivers to the channel, and bumps the
    /// store's fire count. This is the regression guard for #347 — before the
    /// driver existed, no timer ever reached `fire_due`.
    #[tokio::test]
    async fn bounded_tick_fires_due_schedule() {
        let runtime = Arc::new(RecordingRuntime::default());
        let store = Arc::new(InMemoryScheduleStore::new());
        let channel = Arc::new(RecordingChannel::default());
        let loop_ = Arc::new(ProactiveAutomationLoop::new(
            runtime.clone(),
            store.clone(),
            channel.clone(),
        ));

        let schedule = AutomationSchedule::new(
            "every-minute",
            CronExpression::every_minute(),
            SessionId::new(),
            token(),
            budget(),
            "status update",
        );
        let id = schedule.id.clone();
        loop_.upsert_schedule(schedule).await.expect("upsert");

        let driver = ScheduleDriver::new(loop_, Duration::from_millis(10));
        driver.run_bounded(Some(1)).await;

        assert_eq!(
            *runtime.submits.lock().await,
            1,
            "the timer drove one submit"
        );
        assert_eq!(*channel.delivered.lock().await, 1, "the fire was delivered");
        let stored = store.load_all().await.expect("load");
        let fired = stored
            .iter()
            .find(|s| s.id == id)
            .expect("schedule still present");
        assert_eq!(fired.fire_count, 1, "the successful fire was persisted");
    }

    /// A background driver started and then stopped runs at least one real tick
    /// against the executor — proving `start`/`stop` drive execution, not just
    /// lifecycle bookkeeping.
    #[tokio::test]
    async fn background_driver_start_stop_fires() {
        let runtime = Arc::new(RecordingRuntime::default());
        let store = Arc::new(InMemoryScheduleStore::new());
        let channel = Arc::new(RecordingChannel::default());
        let loop_ = Arc::new(ProactiveAutomationLoop::new(
            runtime.clone(),
            store.clone(),
            channel.clone(),
        ));
        let schedule = AutomationSchedule::new(
            "every-minute",
            CronExpression::every_minute(),
            SessionId::new(),
            token(),
            budget(),
            "status update",
        );
        loop_.upsert_schedule(schedule).await.expect("upsert");

        let driver = ScheduleDriver::new(loop_, Duration::from_millis(10));
        assert!(driver.start().await, "first start spawns the driver");
        assert!(
            !driver.start().await,
            "second start is a no-op while running"
        );
        assert!(driver.is_running().await);

        // Give the background ticker time for at least one tick (first tick is
        // immediate, but yield generously to avoid a slow-CI race).
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(driver.stop().await, "stop aborts the running driver");
        assert!(!driver.is_running().await);

        assert!(
            *runtime.submits.lock().await >= 1,
            "the background driver fired the due schedule at least once"
        );
    }
}
