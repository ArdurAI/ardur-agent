//! Executes persisted `ardur schedule` jobs through the real automation
//! executor (issue #347).
//!
//! Before this module, `ardur schedule create` persisted a [`ScheduleRecord`]
//! to `<state>/schedules/<id>.json` and nothing ever ran it: `ardur schedule
//! fire` printed `execution engine not yet wired`, and no timer drove the
//! `ardur-automation` executor (which was fully built but had zero callers).
//! Three disjoint schedule stores existed and none was connected to execution.
//!
//! This module reconciles that. The `<state>/schedules` directory is the single
//! durable store; [`CliScheduleStore`] adapts it to the automation crate's
//! [`ScheduleStore`] contract by *materializing* each persisted record into an
//! [`AutomationSchedule`] at fire time — minting a fresh, short-lived,
//! **attenuated** cap-token and a per-fire budget top-up for every unattended
//! fire (a token minted at *create* time would have expired by the time a daily
//! job fires days later, so per-fire minting is the only correct design for a
//! durable scheduler). Fires then run through the ordinary
//! [`FusedRuntime`](ardur_fused_runtime::FusedRuntime) ten-stage pipeline —
//! cap-token verify → Cedar → cost admission → provider → signed receipt →
//! journal — exactly like an `ardur chat` turn, and successful responses are
//! delivered to a caller-supplied [`AutomationChannel`].
//!
//! Two entry points drive it:
//! - [`run_schedule_fire`] — fire one persisted schedule now, end-to-end
//!   (backs `ardur schedule fire <id>`).
//! - [`run_schedule_run`] — drive every *due* schedule on an interval via
//!   [`ScheduleDriver`] (backs `ardur schedule run`), bounded by `--max-ticks`
//!   or unbounded.
//!
//! The provider/runtime wiring here mirrors [`crate::FusedEngine`]'s builder:
//! the same provider selection (with an offline stub fallback when credentials
//! are absent), the same persistent issuer/receipt keys and Cedar policies, and
//! the same file-backed receipt log — so a fired schedule appends to the very
//! same signed receipt chain a chat turn would.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ardur_automation::{
    AutomationAttenuation, AutomationChannel, AutomationDeliveryEvent, AutomationSchedule,
    AutomationScheduleId, AutomationScheduleStatus, FireReport, FusedAutomationRuntime,
    ProactiveAutomationError, ProactiveAutomationLoop, ScheduleDriver, ScheduleStore,
    ScheduledCapToken,
};
use ardur_cap_token::{BiscuitCapTokenIssuer, CapScope, CapTokenIssuer, HolderId as CapHolderId};
use ardur_cost_gate::{CostEnvelope, CostTuple as GateCostTuple, HolderId as GateHolderId};
use ardur_cron::CronExpression;
use ardur_fused_runtime::{FusedRuntime, FusedRuntimeBuilder};
use ardur_memory::InMemoryMemoryRuntime;
use ardur_provider_runtime::{
    AnthropicProvider, InstrumentedProvider, ModelId, Provider, ProviderError,
};
use ardur_provider_selector as provider_selector;
use ardur_runtime::{CapTokenRef, SessionId};
use ardur_session_journals::FileSessionJournal;
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::CliError;
use crate::secure_io::{read_string_no_follow, write_private_file_atomic_no_follow};
use crate::state::StateDirs;

/// The audience unattended-fire cap-tokens are scoped to — the same audience
/// [`crate::FusedEngine`] verifies chat turns against, so the scheduler runtime
/// accepts them.
const SCHEDULE_AUDIENCE: &str = "cli";
/// The tool/capability every scheduled turn exercises.
const SCHEDULE_TOOL: &str = "chat.submit";
/// A fixed scheduler-process journal session id (a stable v5-ish constant), so
/// all fires from one process share one journal file. Individual fires still
/// carry their own per-schedule `session_id` in the receipt/request.
const SCHEDULER_JOURNAL_SESSION_UUID: u128 = 0x5c4ed000_0000_4000_8000_000000000001;
/// A per-fire attenuated cap-token lives only long enough to run one turn.
const FIRE_CAP_TTL_SECS: u64 = 300;
/// Per-fire cents ceiling admitted onto the schedule subject before a fire. The
/// projected envelope gates only the cents axis, so this bounds a single fire's
/// spend.
const DEFAULT_PER_FIRE_CENTS: u64 = 100;
/// Default driver tick interval (seconds) for `ardur schedule run`.
pub const DEFAULT_DRIVER_INTERVAL_SECS: u64 = 60;

/// A persisted schedule record under `<state>/schedules/<id>.json`.
///
/// This is the single durable schedule shape the CLI reads and writes;
/// `ardur schedule create` writes it, and [`CliScheduleStore`] materializes it
/// into an executable [`AutomationSchedule`]. The `last_fire_at` / `fire_count`
/// fields are additive (serde-defaulted) so records written before #347 load
/// unchanged.
#[derive(Clone, Serialize, Deserialize)]
pub struct ScheduleRecord {
    /// Stable schedule id (a UUID string; also the file stem).
    pub schedule_id: String,
    /// Human-readable label.
    pub label: String,
    /// Five-field cron pattern the schedule fires on.
    pub pattern: String,
    /// Prompt submitted as the user turn on each fire.
    pub prompt: String,
    /// Creation time (unix seconds).
    pub created_at: u64,
    /// Whether the schedule is eligible to fire.
    pub enabled: bool,
    /// Last successful fire time, persisted so a driver does not double-fire
    /// within one cron minute across ticks/restarts.
    #[serde(default)]
    pub last_fire_at: Option<DateTime<Utc>>,
    /// Number of successful fires.
    #[serde(default)]
    pub fire_count: u64,
}

impl ScheduleRecord {
    /// The on-disk path for this record under `dir`.
    fn path_in(dir: &Path, id: &str) -> PathBuf {
        dir.join(format!("{id}.json"))
    }
}

/// Whether `id` is a safe on-disk file stem (no path traversal). Schedule ids
/// are UUIDs, so this rejects anything that is not a bare
/// `[A-Za-z0-9._-]` token — the same guarantee `sanitize_state_id` gives the
/// other state commands.
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && id != "."
        && id != ".."
}

/// Read every persisted schedule record from `dir`, skipping unreadable or
/// malformed files.
pub fn read_schedule_records(dir: &Path) -> Vec<ScheduleRecord> {
    let mut records = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                if let Ok(content) = read_string_no_follow(&path) {
                    if let Ok(record) = serde_json::from_str::<ScheduleRecord>(&content) {
                        records.push(record);
                    }
                }
            }
        }
    }
    records
}

fn write_schedule_record(dir: &Path, record: &ScheduleRecord) -> Result<(), std::io::Error> {
    std::fs::create_dir_all(dir)?;
    let path = ScheduleRecord::path_in(dir, &record.schedule_id);
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_private_file_atomic_no_follow(&path, &bytes)
}

/// Parse a five-field cron pattern string into a validated [`CronExpression`].
fn pattern_to_expression(pattern: &str) -> Result<CronExpression, ProactiveAutomationError> {
    let fields: Vec<&str> = pattern.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(ProactiveAutomationError::InvalidSchedule(format!(
            "cron pattern must have 5 fields, got {}",
            fields.len()
        )));
    }
    let expr = CronExpression::new(fields[0], fields[1], fields[2], fields[3], fields[4]);
    expr.validate().map_err(|e| {
        ProactiveAutomationError::InvalidSchedule(format!("invalid cron `{pattern}`: {e}"))
    })?;
    Ok(expr)
}

/// Serialize a [`CronExpression`] back to its five-field pattern string.
fn expression_to_pattern(expr: &CronExpression) -> String {
    format!(
        "{} {} {} {} {}",
        expr.minute, expr.hour, expr.day_of_month, expr.month, expr.day_of_week
    )
}

/// A [`ScheduleStore`] backed by the CLI's `<state>/schedules/*.json` records.
///
/// `load_all` materializes each record into an executable [`AutomationSchedule`]
/// — minting a fresh attenuated cap-token and per-fire budget for every fire —
/// so the automation executor can run persisted jobs. `record_successful_fire`
/// persists the updated `last_fire_at`/`fire_count` back to the record file.
pub struct CliScheduleStore {
    dir: PathBuf,
    issuer: Arc<BiscuitCapTokenIssuer>,
    subject: String,
    per_fire_cents: u64,
}

impl CliScheduleStore {
    /// Build a store over `dir`, minting per-fire cap-tokens with `issuer` for
    /// `subject`.
    #[must_use]
    pub fn new(
        dir: PathBuf,
        issuer: Arc<BiscuitCapTokenIssuer>,
        subject: String,
        per_fire_cents: u64,
    ) -> Self {
        Self {
            dir,
            issuer,
            subject,
            per_fire_cents,
        }
    }

    /// Mint a fresh, short-lived, attenuated cap-token for one unattended fire.
    fn mint_fire_token(&self) -> Result<ScheduledCapToken, ProactiveAutomationError> {
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cap = self
            .issuer
            .issue(
                CapHolderId(self.subject.clone()),
                CapScope {
                    audience: SCHEDULE_AUDIENCE.to_string(),
                    expires_unix: now_unix + FIRE_CAP_TTL_SECS,
                    budget_remaining: self.per_fire_cents.max(1),
                    tool_allowlist: vec![
                        SCHEDULE_TOOL.to_string(),
                        ardur_memory::MEMORY_READ_CAPABILITY.to_string(),
                        ardur_memory::MEMORY_WRITE_CAPABILITY.to_string(),
                    ],
                },
            )
            .map_err(|e| {
                ProactiveAutomationError::InvalidSchedule(format!(
                    "minting unattended-fire cap-token: {e}"
                ))
            })?;
        let token = CapTokenRef(cap.to_base64().map_err(|e| {
            ProactiveAutomationError::InvalidSchedule(format!(
                "serializing unattended-fire cap-token: {e}"
            ))
        })?);
        Ok(ScheduledCapToken::attenuated(
            token,
            vec![AutomationAttenuation {
                rule: format!("restrict_tools:{SCHEDULE_TOOL}"),
                evidence: Some("ardur schedule unattended fire".to_string()),
            }],
        ))
    }

    fn per_fire_budget(&self) -> GateCostTuple {
        GateCostTuple {
            tokens_in: 1_000_000,
            tokens_out: 1_000_000,
            cents: self.per_fire_cents,
            wall_ms: 1_000_000,
            attention_score: 1_000_000,
        }
    }

    /// Materialize one persisted record into an executable schedule.
    fn materialize(
        &self,
        record: &ScheduleRecord,
    ) -> Result<AutomationSchedule, ProactiveAutomationError> {
        let expression = pattern_to_expression(&record.pattern)?;
        // A record id is a UUID; reuse it as the fire's session id so receipts
        // and journals attribute the fire to a stable session. Fall back to a
        // fresh session id if a legacy record carried a non-UUID id.
        let session_id = uuid::Uuid::parse_str(&record.schedule_id)
            .map(SessionId)
            .unwrap_or_default();
        let mut schedule = AutomationSchedule::new(
            record.label.clone(),
            expression,
            session_id,
            self.mint_fire_token()?,
            self.per_fire_budget(),
            record.prompt.clone(),
        );
        schedule.id = AutomationScheduleId(record.schedule_id.clone());
        schedule.status = if record.enabled {
            AutomationScheduleStatus::Enabled
        } else {
            AutomationScheduleStatus::Paused
        };
        schedule.created_at = Utc
            .timestamp_opt(record.created_at as i64, 0)
            .single()
            .unwrap_or_else(Utc::now);
        schedule.last_fire_at = record.last_fire_at;
        schedule.fire_count = record.fire_count;
        Ok(schedule)
    }

    /// The record shape for an [`AutomationSchedule`] (the lossy inverse of
    /// [`materialize`](Self::materialize): the cap-token/budget are re-minted on
    /// load, so only the durable fields round-trip).
    fn to_record(schedule: &AutomationSchedule) -> ScheduleRecord {
        ScheduleRecord {
            schedule_id: schedule.id.0.clone(),
            label: schedule.name.clone(),
            pattern: expression_to_pattern(&schedule.expression),
            prompt: schedule.prompt.clone(),
            created_at: u64::try_from(schedule.created_at.timestamp().max(0)).unwrap_or(0),
            enabled: schedule.status == AutomationScheduleStatus::Enabled,
            last_fire_at: schedule.last_fire_at,
            fire_count: schedule.fire_count,
        }
    }
}

#[async_trait]
impl ScheduleStore for CliScheduleStore {
    async fn load_all(&self) -> Result<Vec<AutomationSchedule>, ProactiveAutomationError> {
        let mut schedules = Vec::new();
        for record in read_schedule_records(&self.dir) {
            match self.materialize(&record) {
                Ok(schedule) => schedules.push(schedule),
                Err(err) => {
                    tracing::warn!(
                        schedule = %record.schedule_id,
                        error = %err,
                        "skipping unmaterializable schedule record"
                    );
                }
            }
        }
        Ok(schedules)
    }

    async fn upsert(&self, schedule: AutomationSchedule) -> Result<(), ProactiveAutomationError> {
        write_schedule_record(&self.dir, &Self::to_record(&schedule))?;
        Ok(())
    }

    async fn remove(&self, id: &AutomationScheduleId) -> Result<(), ProactiveAutomationError> {
        if !is_safe_id(&id.0) {
            return Err(ProactiveAutomationError::ScheduleNotFound(id.0.clone()));
        }
        let path = ScheduleRecord::path_in(&self.dir, &id.0);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Err(ProactiveAutomationError::ScheduleNotFound(id.0.clone()))
            }
            Err(err) => Err(err.into()),
        }
    }

    async fn record_successful_fire(
        &self,
        id: &AutomationScheduleId,
        fired_at: DateTime<Utc>,
    ) -> Result<AutomationSchedule, ProactiveAutomationError> {
        if !is_safe_id(&id.0) {
            return Err(ProactiveAutomationError::ScheduleNotFound(id.0.clone()));
        }
        let path = ScheduleRecord::path_in(&self.dir, &id.0);
        let raw = match read_string_no_follow(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(ProactiveAutomationError::ScheduleNotFound(id.0.clone()));
            }
            Err(err) => return Err(err.into()),
        };
        let mut record: ScheduleRecord = serde_json::from_str(&raw)?;
        record.last_fire_at = Some(fired_at);
        record.fire_count = record.fire_count.saturating_add(1);
        write_schedule_record(&self.dir, &record)?;
        self.materialize(&record)
    }
}

/// An [`AutomationChannel`] that prints a delivered fire's response to stdout —
/// the CLI's "delivery" for an unattended fire the operator is watching.
struct StdoutAutomationChannel;

#[async_trait]
impl AutomationChannel for StdoutAutomationChannel {
    async fn deliver(
        &self,
        event: AutomationDeliveryEvent,
    ) -> Result<(), ProactiveAutomationError> {
        println!(
            "fired schedule {} ({}) at {}",
            event.schedule_id.0, event.schedule_name, event.fired_at
        );
        println!("  receipt: {}", event.result.receipt_id.0);
        println!("  response: {}", event.result.response.content);
        Ok(())
    }
}

/// The fused runtime plus the issuer/subject a fired schedule needs.
struct SchedulerRuntime {
    runtime: Arc<FusedRuntime>,
    issuer: Arc<BiscuitCapTokenIssuer>,
    subject: String,
    /// Whether the selected provider fell back to the offline stub (no creds).
    offline: bool,
}

/// Build the fused runtime the scheduler fires through. Mirrors
/// [`crate::FusedEngine`]'s builder: provider selection with an offline stub
/// fallback, persistent issuer/receipt keys and Cedar policies, a file-backed
/// receipt log, and a generously provisioned subject budget (the per-fire
/// top-up is layered on at submit time by each schedule's provisioning).
async fn build_scheduler_runtime(
    config: &Config,
    dirs: &StateDirs,
    budget_cents: u64,
) -> Result<SchedulerRuntime, CliError> {
    let model = ModelId::new(&config.model);

    let (provider, offline): (Arc<dyn Provider>, bool) =
        match provider_selector::from_env(model.clone()) {
            Ok(live) => (live, false),
            Err(e @ ProviderError::InvalidSelection(_)) => return Err(CliError::Provider(e)),
            Err(_) => {
                let stub: Arc<dyn Provider> = Arc::new(AnthropicProvider::stub(model.clone()));
                tracing::info!(
                    offline = true,
                    "selected provider unavailable; scheduler using offline stub"
                );
                (stub, true)
            }
        };
    let provider = InstrumentedProvider::wrap(provider);

    let issuer = Arc::new(dirs.load_or_create_issuer()?);
    let cap_root = issuer.public_key();
    let receipt_key = dirs.load_or_create_receipt_key()?;
    let policies = dirs.load_cedar_policies()?;
    let subject = dirs.local_subject();
    let holder = GateHolderId(subject.clone());

    // Gate only the cents axis per fire, like the chat path, so a cents-scoped
    // per-fire top-up covers one turn.
    let envelope = CostEnvelope {
        tokens_in_max: 0,
        tokens_out_max: 0,
        cents_max: u32::try_from(DEFAULT_PER_FIRE_CENTS).unwrap_or(u32::MAX),
        wall_ms_max: 0,
        attention_score_max: 0,
    };

    let journal_session = SessionId(uuid::Uuid::from_u128(SCHEDULER_JOURNAL_SESSION_UUID));
    let journal = FileSessionJournal::new(&dirs.journals, journal_session)
        .map_err(|e| CliError::State(format!("opening the scheduler journal: {e}")))?;
    let memory = Arc::new(InMemoryMemoryRuntime::new());

    let (runtime, _reconciliation) =
        FusedRuntimeBuilder::new(cap_root, policies, provider, receipt_key, model)
            .audience(SCHEDULE_AUDIENCE)
            .tool(SCHEDULE_TOOL)
            .provision_budget(
                holder,
                GateCostTuple {
                    tokens_in: 1_000_000_000,
                    tokens_out: 1_000_000_000,
                    cents: budget_cents.max(DEFAULT_PER_FIRE_CENTS),
                    wall_ms: 1_000_000_000,
                    attention_score: 1_000_000_000,
                },
            )
            .projected_envelope(envelope)
            .with_memory(memory)
            .with_journal(Arc::new(journal))
            .with_default_injection_filters()
            .receipt_log(dirs.receipt_log())
            .build_reconciled()
            .await
            .map_err(|e| CliError::State(format!("building the scheduler runtime: {e}")))?;

    Ok(SchedulerRuntime {
        runtime: Arc::new(runtime),
        issuer,
        subject,
        offline,
    })
}

/// Assemble the automation loop over the CLI schedule store, the scheduler
/// runtime, and the stdout delivery channel.
type CliLoop =
    ProactiveAutomationLoop<FusedAutomationRuntime, CliScheduleStore, StdoutAutomationChannel>;

fn build_loop(scheduler: &SchedulerRuntime, dirs: &StateDirs) -> Arc<CliLoop> {
    let store = CliScheduleStore::new(
        dirs.root.join("schedules"),
        Arc::clone(&scheduler.issuer),
        scheduler.subject.clone(),
        DEFAULT_PER_FIRE_CENTS,
    );
    let runtime = FusedAutomationRuntime::new(Arc::clone(&scheduler.runtime));
    Arc::new(ProactiveAutomationLoop::new(
        Arc::new(runtime),
        Arc::new(store),
        Arc::new(StdoutAutomationChannel),
    ))
}

/// Ensure the state directories a fire touches (keys, journals, receipts,
/// schedules) exist. Deliberately does *not* run the state-tree schema
/// migration [`StateDirs::create`] performs: `ardur schedule create` never
/// stamps the schema either, and the fire path must not fail when it is the
/// first `ardur` command to touch the state tree (e.g. an unattended driver
/// booting before any interactive session).
fn prepare_dirs(dirs: &StateDirs) -> Result<(), CliError> {
    let schedules = dirs.root.join("schedules");
    for dir in [&dirs.keys, &dirs.journals, &dirs.receipts, &schedules] {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

/// Fire one persisted schedule now, end-to-end. Backs `ardur schedule fire`.
///
/// # Errors
/// If the id is unknown, the state tree cannot be prepared, the runtime cannot
/// be built, or the fire itself fails (a delivered-but-errored fire is reported
/// as an error so the operator sees non-zero exit and the reason).
pub fn run_schedule_fire(dirs: &StateDirs, config: &Config, id: &str) -> Result<(), CliError> {
    install_stderr_tracing();
    if !is_safe_id(id) {
        return Err(CliError::State(format!("invalid schedule id `{id}`")));
    }
    prepare_dirs(dirs)?;
    let schedules_dir = dirs.root.join("schedules");
    if !ScheduleRecord::path_in(&schedules_dir, id).is_file() {
        return Err(CliError::State(format!("schedule `{id}` not found")));
    }

    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    tokio_rt.block_on(async move {
        let scheduler = build_scheduler_runtime(config, dirs, config.budget_cents).await?;
        if scheduler.offline {
            eprintln!(
                "note: no live provider credentials found; firing against the offline stub provider"
            );
        }
        let loop_ = build_loop(&scheduler, dirs);
        let report = loop_
            .fire_now(&AutomationScheduleId(id.to_string()))
            .await
            .map_err(|e| CliError::State(format!("firing schedule `{id}`: {e}")))?;
        report_or_err(id, &report)
    })
}

/// Drive every *due* schedule on an interval. Backs `ardur schedule run`.
///
/// `max_ticks` bounds the number of ticks (`Some(1)` fires everything due right
/// now once and returns); `None` runs until the process is interrupted. The
/// first tick fires immediately.
///
/// # Errors
/// If the state tree cannot be prepared or the runtime cannot be built. A fire
/// that fails mid-run is logged (per schedule) but does not abort the driver.
pub fn run_schedule_run(
    dirs: &StateDirs,
    config: &Config,
    interval_secs: u64,
    max_ticks: Option<usize>,
) -> Result<(), CliError> {
    install_stderr_tracing();
    prepare_dirs(dirs)?;

    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    tokio_rt.block_on(async move {
        let scheduler = build_scheduler_runtime(config, dirs, config.budget_cents).await?;
        if scheduler.offline {
            eprintln!(
                "note: no live provider credentials found; driving schedules against the offline stub provider"
            );
        }
        let loop_ = build_loop(&scheduler, dirs);
        let interval = Duration::from_secs(interval_secs.max(1));
        let driver = ScheduleDriver::new(loop_, interval);
        match max_ticks {
            Some(ticks) => println!(
                "driving due schedules every {interval_secs}s for {ticks} tick(s)"
            ),
            None => println!(
                "driving due schedules every {interval_secs}s (Ctrl-C to stop)"
            ),
        }
        driver.run_bounded(max_ticks).await;
        Ok::<(), CliError>(())
    })
}

/// Turn a [`FireReport`] into a printed success or a [`CliError`].
fn report_or_err(id: &str, report: &FireReport) -> Result<(), CliError> {
    if report.delivered {
        Ok(())
    } else {
        Err(CliError::State(format!(
            "schedule `{id}` did not fire: {}",
            report.error.as_deref().unwrap_or("unknown error")
        )))
    }
}

/// Install a stderr tracing subscriber if none is set (a no-op otherwise), so
/// scheduler warnings surface without clobbering an already-installed one.
fn install_stderr_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_safe_id_rejects_traversal() {
        assert!(is_safe_id("59d683b9-0000-4000-8000-000000000000"));
        assert!(!is_safe_id("../etc/passwd"));
        assert!(!is_safe_id("a/b"));
        assert!(!is_safe_id(".."));
        assert!(!is_safe_id(""));
    }

    #[test]
    fn pattern_round_trips_through_expression() {
        let expr = pattern_to_expression("0,5 * * * *").expect("valid pattern");
        assert_eq!(expression_to_pattern(&expr), "0,5 * * * *");
        assert!(pattern_to_expression("bad").is_err());
        assert!(pattern_to_expression("99 * * * *").is_err());
    }

    #[test]
    fn record_fire_fields_default_on_legacy_json() {
        // A pre-#347 record (no last_fire_at / fire_count) still deserializes.
        let legacy = r#"{
            "schedule_id": "id-1",
            "label": "daily",
            "pattern": "0 9 * * *",
            "prompt": "post standup",
            "created_at": 1750000000,
            "enabled": true
        }"#;
        let record: ScheduleRecord = serde_json::from_str(legacy).expect("legacy record loads");
        assert_eq!(record.fire_count, 0);
        assert!(record.last_fire_at.is_none());
    }
}
