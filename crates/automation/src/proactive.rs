//! Proactive automation loop for scheduled/triggered jobs.
//!
//! ARD-458 promotes the §9.x automation surface from a contract-only task-flow
//! shim into a real, durable scheduler seam: schedules are persisted through a
//! [`ScheduleStore`], due fires are submitted through the fused runtime under a
//! per-fire attenuated cap-token and budget top-up, and successful responses are
//! delivered to a caller-provided channel sink. ARD-242 is the related product
//! reference for proactive assistant behavior.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ardur_cost_gate::{CostTuple as GateCostTuple, HolderId as GateHolderId};
use ardur_cron::CronExpression;
use ardur_fused_runtime::{FusedRuntime, PerRequestProvisioning};
use ardur_runtime::{
    CapTokenRef, ChatMessage, RuntimeError, SessionId, SubmitRequest, SubmitResult,
};
use ardur_standing_goals::{Frequency, GoalId, StandingGoal};
use async_trait::async_trait;
use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

/// Stable id for an automation schedule.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AutomationScheduleId(pub String);

impl AutomationScheduleId {
    /// Mint a fresh time-ordered schedule id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7().to_string())
    }
}

impl Default for AutomationScheduleId {
    fn default() -> Self {
        Self::new()
    }
}

/// Proof that the schedule's cap-token has been narrowed for unattended work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationAttenuation {
    /// Human-readable attenuation rule, e.g. `restrict_tools:chat.submit`.
    pub rule: String,
    /// Optional evidence reference for the attenuation operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// A cap-token that may be used by the automation loop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledCapToken {
    /// The token handle presented to [`FusedRuntime::submit_with_provisioning`].
    pub token: CapTokenRef,
    /// Append-only attenuation evidence. The loop refuses schedules without at
    /// least one entry so unattended fires cannot accidentally use a root token.
    pub attenuation_chain: Vec<AutomationAttenuation>,
}

impl ScheduledCapToken {
    /// Construct a scheduled token from a handle and its attenuation proof.
    #[must_use]
    pub fn attenuated(token: CapTokenRef, attenuation_chain: Vec<AutomationAttenuation>) -> Self {
        Self {
            token,
            attenuation_chain,
        }
    }

    /// Whether this token carries attenuation evidence.
    #[must_use]
    pub fn is_attenuated(&self) -> bool {
        !self.attenuation_chain.is_empty()
    }
}

/// Schedule lifecycle state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationScheduleStatus {
    /// Schedule is eligible to fire when due.
    Enabled,
    /// Schedule is persisted but skipped by the loop.
    Paused,
    /// Schedule completed and should not fire again.
    Completed,
}

/// Durable proactive automation schedule.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutomationSchedule {
    /// Stable schedule id.
    pub id: AutomationScheduleId,
    /// Human-readable name.
    pub name: String,
    /// Cron expression used to decide whether a tick is due.
    pub expression: CronExpression,
    /// Optional standing-goal id that authored this schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standing_goal_id: Option<GoalId>,
    /// Session used for all fires from this schedule.
    pub session_id: SessionId,
    /// Optional explicit audience override for cap-token verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    /// Optional budget holder override for this schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_subject: Option<String>,
    /// Per-fire budget top-up admitted by the fused runtime cost gate.
    pub per_fire_budget: GateCostTuple,
    /// Attenuated token used for unattended fires.
    pub cap_token: ScheduledCapToken,
    /// Optional system instruction prepended to the generated turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// User prompt submitted on each fire.
    pub prompt: String,
    /// Current schedule status.
    pub status: AutomationScheduleStatus,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last successful fire timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fire_at: Option<DateTime<Utc>>,
    /// Number of successful fires.
    pub fire_count: u64,
    /// User metadata preserved with the schedule.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

impl AutomationSchedule {
    /// Create a new enabled schedule.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        expression: CronExpression,
        session_id: SessionId,
        cap_token: ScheduledCapToken,
        per_fire_budget: GateCostTuple,
        prompt: impl Into<String>,
    ) -> Self {
        Self {
            id: AutomationScheduleId::new(),
            name: name.into(),
            expression,
            standing_goal_id: None,
            session_id,
            audience: None,
            budget_subject: None,
            per_fire_budget,
            cap_token,
            system_prompt: None,
            prompt: prompt.into(),
            status: AutomationScheduleStatus::Enabled,
            created_at: Utc::now(),
            last_fire_at: None,
            fire_count: 0,
            metadata: HashMap::new(),
        }
    }

    /// Build a schedule from a standing goal's frequency and text.
    #[must_use]
    pub fn from_standing_goal(
        goal: &StandingGoal,
        session_id: SessionId,
        cap_token: ScheduledCapToken,
        per_fire_budget: GateCostTuple,
    ) -> Self {
        let prompt = format!(
            "Proactively advance standing goal `{}`.\n\n{}",
            goal.title, goal.description
        );
        let mut schedule = Self::new(
            goal.title.clone(),
            cron_for_frequency(&goal.frequency),
            session_id,
            cap_token,
            per_fire_budget,
            prompt,
        );
        schedule.standing_goal_id = Some(goal.id.clone());
        schedule.status = match goal.status {
            ardur_standing_goals::GoalStatus::Active => AutomationScheduleStatus::Enabled,
            ardur_standing_goals::GoalStatus::Paused => AutomationScheduleStatus::Paused,
            ardur_standing_goals::GoalStatus::Completed
            | ardur_standing_goals::GoalStatus::Abandoned => AutomationScheduleStatus::Completed,
        };
        schedule
    }

    /// Return true when this schedule should fire at `now`.
    #[must_use]
    pub fn is_due(&self, now: DateTime<Utc>) -> bool {
        if self.status != AutomationScheduleStatus::Enabled || !self.expression.is_due(now) {
            return false;
        }
        !self
            .last_fire_at
            .is_some_and(|last| same_cron_minute(last, now))
    }

    /// Convert this schedule into the runtime request submitted for one fire.
    #[must_use]
    pub fn submit_request(&self) -> SubmitRequest {
        let mut messages = Vec::new();
        if let Some(system_prompt) = &self.system_prompt {
            messages.push(ChatMessage::system(system_prompt.clone()));
        }
        messages.push(ChatMessage::user(self.prompt.clone()));
        SubmitRequest {
            messages,
            cap_token: self.cap_token.token.clone(),
            session_id: self.session_id,
            requested_provider: None,
        }
    }

    /// Build per-request provisioning used by the fused runtime's cost gate.
    #[must_use]
    pub fn provisioning(&self) -> PerRequestProvisioning {
        PerRequestProvisioning {
            budget: Some(self.per_fire_budget),
            audience: self.audience.clone(),
            subject: self.budget_subject.clone().map(GateHolderId),
        }
    }

    fn record_success(&mut self, now: DateTime<Utc>) {
        self.last_fire_at = Some(now);
        self.fire_count = self.fire_count.saturating_add(1);
    }
}

/// Result emitted after an automation fire is delivered.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutomationDeliveryEvent {
    /// Schedule that fired.
    pub schedule_id: AutomationScheduleId,
    /// Schedule name at fire time.
    pub schedule_name: String,
    /// Time the schedule was fired.
    pub fired_at: DateTime<Utc>,
    /// Runtime result that was delivered to the channel.
    pub result: SubmitResult,
}

/// Per-schedule fire report.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FireReport {
    /// Schedule that was considered.
    pub schedule_id: AutomationScheduleId,
    /// Whether the runtime submit and channel delivery both succeeded.
    pub delivered: bool,
    /// Error string when the fire failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Error surface for the proactive automation loop.
#[derive(Debug, thiserror::Error)]
pub enum ProactiveAutomationError {
    /// Schedule was not found in the durable store.
    #[error("schedule not found: {0}")]
    ScheduleNotFound(String),
    /// The schedule is invalid.
    #[error("invalid schedule: {0}")]
    InvalidSchedule(String),
    /// Persistence failed.
    #[error("schedule store I/O failed: {0}")]
    StoreIo(#[from] std::io::Error),
    /// Serialization failed.
    #[error("schedule store serialization failed: {0}")]
    StoreSerde(#[from] serde_json::Error),
    /// Runtime submit failed.
    #[error("runtime submit failed: {0}")]
    Runtime(#[source] RuntimeError),
    /// Channel delivery failed.
    #[error("channel delivery failed: {0}")]
    Delivery(String),
}

/// Durable schedule store abstraction.
#[async_trait]
pub trait ScheduleStore: Send + Sync + 'static {
    /// Load all schedules.
    async fn load_all(&self) -> Result<Vec<AutomationSchedule>, ProactiveAutomationError>;
    /// Persist or replace one schedule.
    async fn upsert(&self, schedule: AutomationSchedule) -> Result<(), ProactiveAutomationError>;
    /// Remove one schedule.
    async fn remove(&self, id: &AutomationScheduleId) -> Result<(), ProactiveAutomationError>;
    /// Mark a schedule as successfully fired.
    async fn record_successful_fire(
        &self,
        id: &AutomationScheduleId,
        fired_at: DateTime<Utc>,
    ) -> Result<AutomationSchedule, ProactiveAutomationError>;
}

/// File-backed JSON schedule store.
#[derive(Debug)]
pub struct FileScheduleStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileScheduleStore {
    /// Create a store backed by `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    async fn read_locked(&self) -> Result<Vec<AutomationSchedule>, ProactiveAutomationError> {
        match tokio::fs::read(&self.path).await {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(err) => Err(err.into()),
        }
    }

    async fn write_locked(
        &self,
        schedules: &[AutomationSchedule],
    ) -> Result<(), ProactiveAutomationError> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let data = serde_json::to_vec_pretty(schedules)?;
        let tmp = temporary_path(&self.path);
        tokio::fs::write(&tmp, data).await?;
        tokio::fs::rename(&tmp, &self.path).await?;
        Ok(())
    }
}

#[async_trait]
impl ScheduleStore for FileScheduleStore {
    async fn load_all(&self) -> Result<Vec<AutomationSchedule>, ProactiveAutomationError> {
        let _guard = self.lock.lock().await;
        self.read_locked().await
    }

    async fn upsert(&self, schedule: AutomationSchedule) -> Result<(), ProactiveAutomationError> {
        validate_schedule(&schedule)?;
        let _guard = self.lock.lock().await;
        let mut schedules = self.read_locked().await?;
        if let Some(existing) = schedules.iter_mut().find(|item| item.id == schedule.id) {
            *existing = schedule;
        } else {
            schedules.push(schedule);
        }
        self.write_locked(&schedules).await
    }

    async fn remove(&self, id: &AutomationScheduleId) -> Result<(), ProactiveAutomationError> {
        let _guard = self.lock.lock().await;
        let mut schedules = self.read_locked().await?;
        let before = schedules.len();
        schedules.retain(|schedule| &schedule.id != id);
        if schedules.len() == before {
            return Err(ProactiveAutomationError::ScheduleNotFound(id.0.clone()));
        }
        self.write_locked(&schedules).await
    }

    async fn record_successful_fire(
        &self,
        id: &AutomationScheduleId,
        fired_at: DateTime<Utc>,
    ) -> Result<AutomationSchedule, ProactiveAutomationError> {
        let _guard = self.lock.lock().await;
        let mut schedules = self.read_locked().await?;
        let schedule = schedules
            .iter_mut()
            .find(|schedule| &schedule.id == id)
            .ok_or_else(|| ProactiveAutomationError::ScheduleNotFound(id.0.clone()))?;
        schedule.record_success(fired_at);
        let updated = schedule.clone();
        self.write_locked(&schedules).await?;
        Ok(updated)
    }
}

/// In-memory schedule store for tests and embedded deployments.
#[derive(Debug, Default)]
pub struct InMemoryScheduleStore {
    schedules: Mutex<HashMap<AutomationScheduleId, AutomationSchedule>>,
}

impl InMemoryScheduleStore {
    /// Create an empty in-memory store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ScheduleStore for InMemoryScheduleStore {
    async fn load_all(&self) -> Result<Vec<AutomationSchedule>, ProactiveAutomationError> {
        Ok(self.schedules.lock().await.values().cloned().collect())
    }

    async fn upsert(&self, schedule: AutomationSchedule) -> Result<(), ProactiveAutomationError> {
        validate_schedule(&schedule)?;
        self.schedules
            .lock()
            .await
            .insert(schedule.id.clone(), schedule);
        Ok(())
    }

    async fn remove(&self, id: &AutomationScheduleId) -> Result<(), ProactiveAutomationError> {
        self.schedules
            .lock()
            .await
            .remove(id)
            .ok_or_else(|| ProactiveAutomationError::ScheduleNotFound(id.0.clone()))?;
        Ok(())
    }

    async fn record_successful_fire(
        &self,
        id: &AutomationScheduleId,
        fired_at: DateTime<Utc>,
    ) -> Result<AutomationSchedule, ProactiveAutomationError> {
        let mut schedules = self.schedules.lock().await;
        let schedule = schedules
            .get_mut(id)
            .ok_or_else(|| ProactiveAutomationError::ScheduleNotFound(id.0.clone()))?;
        schedule.record_success(fired_at);
        Ok(schedule.clone())
    }
}

/// Runtime abstraction used by the loop.
#[async_trait]
pub trait AutomationRuntime: Send + Sync + 'static {
    /// Submit one proactive turn under the supplied provisioning.
    async fn submit(
        &self,
        req: SubmitRequest,
        provisioning: PerRequestProvisioning,
    ) -> Result<SubmitResult, RuntimeError>;
}

/// Production runtime adapter that invokes [`FusedRuntime::submit_with_provisioning`].
#[derive(Clone)]
pub struct FusedAutomationRuntime {
    runtime: Arc<FusedRuntime>,
}

impl FusedAutomationRuntime {
    /// Wrap a fused runtime for proactive automation.
    #[must_use]
    pub fn new(runtime: Arc<FusedRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl AutomationRuntime for FusedAutomationRuntime {
    async fn submit(
        &self,
        req: SubmitRequest,
        provisioning: PerRequestProvisioning,
    ) -> Result<SubmitResult, RuntimeError> {
        self.runtime
            .submit_with_provisioning(req, provisioning)
            .await
    }
}

/// Channel abstraction used to deliver successful automation results.
#[async_trait]
pub trait AutomationChannel: Send + Sync + 'static {
    /// Deliver one successful fire event.
    async fn deliver(&self, event: AutomationDeliveryEvent)
    -> Result<(), ProactiveAutomationError>;
}

/// Tokio mpsc channel sink for automation delivery events.
#[derive(Clone, Debug)]
pub struct MpscAutomationChannel {
    sender: mpsc::Sender<AutomationDeliveryEvent>,
}

impl MpscAutomationChannel {
    /// Create a channel sink from an mpsc sender.
    #[must_use]
    pub fn new(sender: mpsc::Sender<AutomationDeliveryEvent>) -> Self {
        Self { sender }
    }
}

#[async_trait]
impl AutomationChannel for MpscAutomationChannel {
    async fn deliver(
        &self,
        event: AutomationDeliveryEvent,
    ) -> Result<(), ProactiveAutomationError> {
        self.sender
            .send(event)
            .await
            .map_err(|err| ProactiveAutomationError::Delivery(err.to_string()))
    }
}

/// Proactive automation orchestrator.
#[derive(Clone)]
pub struct ProactiveAutomationLoop<R, S, C> {
    runtime: Arc<R>,
    store: Arc<S>,
    channel: Arc<C>,
}

impl<R, S, C> ProactiveAutomationLoop<R, S, C>
where
    R: AutomationRuntime,
    S: ScheduleStore,
    C: AutomationChannel,
{
    /// Create an orchestrator from its runtime, durable store, and channel sink.
    #[must_use]
    pub fn new(runtime: Arc<R>, store: Arc<S>, channel: Arc<C>) -> Self {
        Self {
            runtime,
            store,
            channel,
        }
    }

    /// Insert or replace a schedule after validating its attenuation and budget.
    pub async fn upsert_schedule(
        &self,
        schedule: AutomationSchedule,
    ) -> Result<(), ProactiveAutomationError> {
        self.store.upsert(schedule).await
    }

    /// Fire every due schedule once for `now`.
    pub async fn fire_due(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<FireReport>, ProactiveAutomationError> {
        let schedules = self.store.load_all().await?;
        let mut reports = Vec::new();
        for schedule in schedules
            .into_iter()
            .filter(|schedule| schedule.is_due(now))
        {
            reports.push(self.fire_schedule(schedule, now).await);
        }
        Ok(reports)
    }

    /// Fire one schedule by id, regardless of its cron due status.
    pub async fn fire_now(
        &self,
        id: &AutomationScheduleId,
    ) -> Result<FireReport, ProactiveAutomationError> {
        let schedule = self
            .store
            .load_all()
            .await?
            .into_iter()
            .find(|schedule| &schedule.id == id)
            .ok_or_else(|| ProactiveAutomationError::ScheduleNotFound(id.0.clone()))?;
        Ok(self.fire_schedule(schedule, Utc::now()).await)
    }

    async fn fire_schedule(
        &self,
        schedule: AutomationSchedule,
        fired_at: DateTime<Utc>,
    ) -> FireReport {
        let id = schedule.id.clone();
        if let Err(err) = validate_schedule(&schedule) {
            return FireReport {
                schedule_id: id,
                delivered: false,
                error: Some(err.to_string()),
            };
        }
        let result = match self
            .runtime
            .submit(schedule.submit_request(), schedule.provisioning())
            .await
        {
            Ok(result) => result,
            Err(err) => {
                return FireReport {
                    schedule_id: id,
                    delivered: false,
                    error: Some(ProactiveAutomationError::Runtime(err).to_string()),
                };
            }
        };
        let event = AutomationDeliveryEvent {
            schedule_id: id.clone(),
            schedule_name: schedule.name.clone(),
            fired_at,
            result,
        };
        if let Err(err) = self.channel.deliver(event).await {
            return FireReport {
                schedule_id: id,
                delivered: false,
                error: Some(err.to_string()),
            };
        }
        if let Err(err) = self.store.record_successful_fire(&id, fired_at).await {
            return FireReport {
                schedule_id: id,
                delivered: false,
                error: Some(err.to_string()),
            };
        }
        FireReport {
            schedule_id: id,
            delivered: true,
            error: None,
        }
    }
}

fn validate_schedule(schedule: &AutomationSchedule) -> Result<(), ProactiveAutomationError> {
    if schedule.name.trim().is_empty() {
        return Err(ProactiveAutomationError::InvalidSchedule(
            "name must not be empty".to_string(),
        ));
    }
    if schedule.prompt.trim().is_empty() {
        return Err(ProactiveAutomationError::InvalidSchedule(
            "prompt must not be empty".to_string(),
        ));
    }
    if !schedule.cap_token.is_attenuated() {
        return Err(ProactiveAutomationError::InvalidSchedule(
            "scheduled fires require an attenuated cap-token".to_string(),
        ));
    }
    if schedule.per_fire_budget == GateCostTuple::ZERO {
        return Err(ProactiveAutomationError::InvalidSchedule(
            "per-fire budget must cover at least one cost dimension".to_string(),
        ));
    }
    Ok(())
}

fn cron_for_frequency(frequency: &Frequency) -> CronExpression {
    match frequency {
        Frequency::Hourly => CronExpression::hourly(),
        Frequency::Daily => CronExpression::daily(),
        Frequency::Weekly => CronExpression::new("0", "0", "*", "*", "0"),
        Frequency::Monthly => CronExpression::new("0", "0", "1", "*", "*"),
        Frequency::Custom(expr) => {
            parse_five_field_cron(expr).unwrap_or_else(CronExpression::daily)
        }
    }
}

fn parse_five_field_cron(expr: &str) -> Option<CronExpression> {
    let parts: Vec<_> = expr.split_whitespace().collect();
    if parts.len() == 5 {
        Some(CronExpression::new(
            parts[0], parts[1], parts[2], parts[3], parts[4],
        ))
    } else {
        None
    }
}

fn same_cron_minute(left: DateTime<Utc>, right: DateTime<Utc>) -> bool {
    left.year() == right.year()
        && left.month() == right.month()
        && left.day() == right.day()
        && left.hour() == right.hour()
        && left.minute() == right.minute()
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut tmp = path.to_path_buf();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| "schedules".to_string(), ToString::to_string);
    tmp.set_file_name(format!(".{file_name}.tmp"));
    tmp
}

#[cfg(test)]
mod tests {
    use super::*;
    use ardur_runtime::{CostTuple as RuntimeCostTuple, ReceiptId};
    use tempfile::tempdir;

    #[derive(Debug, Default)]
    struct RecordingRuntime {
        seen: Mutex<Vec<(SubmitRequest, PerRequestProvisioning)>>,
    }

    #[async_trait]
    impl AutomationRuntime for RecordingRuntime {
        async fn submit(
            &self,
            req: SubmitRequest,
            provisioning: PerRequestProvisioning,
        ) -> Result<SubmitResult, RuntimeError> {
            self.seen.lock().await.push((req, provisioning));
            Ok(SubmitResult {
                receipt_id: ReceiptId::new(),
                response: ChatMessage::assistant("done"),
                cost: RuntimeCostTuple::default(),
            })
        }
    }

    #[derive(Debug, Default)]
    struct RecordingChannel {
        events: Mutex<Vec<AutomationDeliveryEvent>>,
    }

    #[async_trait]
    impl AutomationChannel for RecordingChannel {
        async fn deliver(
            &self,
            event: AutomationDeliveryEvent,
        ) -> Result<(), ProactiveAutomationError> {
            self.events.lock().await.push(event);
            Ok(())
        }
    }

    fn token() -> ScheduledCapToken {
        ScheduledCapToken::attenuated(
            CapTokenRef("attenuated-token".to_string()),
            vec![AutomationAttenuation {
                rule: "restrict_tools:chat.submit".to_string(),
                evidence: Some("test".to_string()),
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

    #[tokio::test]
    async fn file_store_persists_and_records_fire() {
        let dir = tempdir().expect("tempdir");
        let store = FileScheduleStore::new(dir.path().join("schedules.json"));
        let schedule = AutomationSchedule::new(
            "daily",
            CronExpression::daily(),
            SessionId::new(),
            token(),
            budget(),
            "check goal",
        );
        let id = schedule.id.clone();
        store.upsert(schedule).await.expect("upsert");
        assert_eq!(store.load_all().await.expect("load").len(), 1);
        let fired_at = Utc::now();
        let updated = store
            .record_successful_fire(&id, fired_at)
            .await
            .expect("record fire");
        assert_eq!(updated.fire_count, 1);
        assert_eq!(updated.last_fire_at, Some(fired_at));
    }

    #[tokio::test]
    async fn due_fire_submits_with_budget_and_delivers_to_channel() {
        let runtime = Arc::new(RecordingRuntime::default());
        let store = Arc::new(InMemoryScheduleStore::new());
        let channel = Arc::new(RecordingChannel::default());
        let loop_ = ProactiveAutomationLoop::new(runtime.clone(), store.clone(), channel.clone());
        let mut schedule = AutomationSchedule::new(
            "every-minute",
            CronExpression::every_minute(),
            SessionId::new(),
            token(),
            budget(),
            "status update",
        );
        schedule.audience = Some("automation".to_string());
        let id = schedule.id.clone();
        loop_.upsert_schedule(schedule).await.expect("upsert");

        let reports = loop_.fire_due(Utc::now()).await.expect("fire due");

        assert_eq!(reports.len(), 1);
        assert!(reports[0].delivered);
        assert_eq!(reports[0].schedule_id, id);
        assert_eq!(runtime.seen.lock().await.len(), 1);
        assert_eq!(channel.events.lock().await.len(), 1);
        let stored = store.load_all().await.expect("load");
        assert_eq!(stored[0].fire_count, 1);
    }

    #[tokio::test]
    async fn unattenuated_schedule_is_rejected() {
        let store = InMemoryScheduleStore::new();
        let schedule = AutomationSchedule::new(
            "unsafe",
            CronExpression::every_minute(),
            SessionId::new(),
            ScheduledCapToken::attenuated(CapTokenRef("root".to_string()), Vec::new()),
            budget(),
            "run",
        );
        let err = store.upsert(schedule).await.expect_err("must reject");
        assert!(err.to_string().contains("attenuated cap-token"));
    }
}
