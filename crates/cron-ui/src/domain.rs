//! Domain objects for the cron operator UI (§9.4).
//!
//! These mirror the "inspector/controller over the same scheduler state and
//! receipts" shape from the blueprint. A [`CronRecord`] is the durable,
//! owner-scoped persistence shape; [`CronRow`] and [`CronDetail`] are the
//! *rendered* projections that always pass through sentinel redaction before
//! they reach a terminal.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Lifecycle state of a cron as surfaced to the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CronStatus {
    /// Enabled and eligible to fire on schedule.
    Active,
    /// Paused by the operator; retained but not firing.
    Paused,
    /// Last run failed; needs operator attention.
    Errored,
}

impl CronStatus {
    /// A short glyph for compact list rendering.
    pub fn glyph(self) -> &'static str {
        match self {
            CronStatus::Active => "●",
            CronStatus::Paused => "⏸",
            CronStatus::Errored => "✗",
        }
    }
}

/// Outcome of a single cron run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunStatus {
    /// The run completed successfully.
    Success,
    /// The run failed with the given reason.
    Failed {
        /// Operator-facing failure reason (sentinel-scanned before render).
        reason: String,
    },
    /// The run was skipped (e.g. previous run still in flight).
    Skipped,
}

/// A summary of one historical cron run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    /// Stable id of the run.
    pub run_id: String,
    /// When the run started.
    pub started_at: DateTime<Utc>,
    /// Wall-clock duration of the run, in milliseconds.
    pub duration_ms: u64,
    /// The run's outcome.
    pub status: RunStatus,
    /// Cost of the run in whole US cents.
    pub cost_cents: u64,
    /// Head of the run's receipt chain (opaque hex), for `journal find` handoff.
    pub receipt_chain_head: Option<String>,
}

/// Where a cron delivers its output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum DeliveryMode {
    /// Deliver into a chat session.
    ChatSession {
        /// Target session id.
        session_id: String,
    },
    /// Deliver to a channel peer.
    ChannelPeer {
        /// Channel binding (e.g. `slack`, `discord`).
        channel: String,
        /// Peer id within the channel.
        peer: String,
    },
    /// Deliver to a registered outbound webhook endpoint.
    Webhook {
        /// Endpoint URL (sentinel-scanned before render).
        url: String,
    },
    /// Retain output internally only; no external delivery.
    InternalOnly,
}

impl DeliveryMode {
    /// A short label for list rendering.
    pub fn label(&self) -> &'static str {
        match self {
            DeliveryMode::ChatSession { .. } => "chat",
            DeliveryMode::ChannelPeer { .. } => "channel",
            DeliveryMode::Webhook { .. } => "webhook",
            DeliveryMode::InternalOnly => "internal",
        }
    }
}

/// The durable, owner-scoped persistence shape for one cron.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronRecord {
    /// Stable cron id.
    pub id: String,
    /// Operator-facing name.
    pub name: String,
    /// 5-field cron expression (validated on write).
    pub schedule_expr: String,
    /// The prompt/mission the cron dispatches when it fires.
    pub prompt: String,
    /// Lifecycle state.
    pub status: CronStatus,
    /// Fingerprint of the cap-token holder that owns this cron. The `Self_`
    /// visibility tier filters the list to rows whose owner matches the
    /// operator (ADR-Phase3-279).
    pub owner_fingerprint: String,
    /// Delivery target.
    pub delivery_mode: DeliveryMode,
    /// Optional per-cron model override.
    pub model_override: Option<String>,
    /// Optional per-cron thinking-level override.
    pub thinking_override: Option<String>,
    /// Optional mission tag for filtering.
    pub mission_tag: Option<String>,
    /// Optional channel binding for filtering.
    pub channel_binding: Option<String>,
    /// When the cron was created.
    pub created_at: DateTime<Utc>,
    /// When the record was last updated (used for redaction cache keying).
    pub updated_at: DateTime<Utc>,
    /// Bounded ring of recent runs (most recent last).
    pub run_history: Vec<RunSummary>,
    /// Total number of times the cron has fired.
    pub run_count: u64,
}

impl CronRecord {
    /// The most recent run, if any.
    pub fn last_run(&self) -> Option<&RunSummary> {
        self.run_history.last()
    }
}

/// Which crons an operator may see (ADR-Phase3-279).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisibilityTier {
    /// Only crons owned by the operator's cap-token (default).
    #[default]
    SelfOnly,
    /// All crons in the active project scope (requires `cron.ui.project`).
    Project,
    /// All crons in the tenant (requires `cron.ui.admin`).
    Tenant,
}

/// Render density for list output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    /// One line per cron, minimal columns.
    Compact,
    /// Default column set.
    #[default]
    Default,
    /// Extra per-row detail.
    Comfortable,
}

/// The two operating modes (ADR-Phase3-278). Mode is a UX convenience; the
/// authorization check happens at admission of each mutate action, not at mode
/// switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    /// Read-only navigation, filtering, inspection.
    View,
    /// Mutations enabled (still cap-token-gated per action).
    Mutate,
}

/// A projected, redaction-safe list row with computed statistics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CronRow {
    /// Cron id.
    pub id: String,
    /// Redacted name.
    pub name: String,
    /// Cron expression.
    pub schedule_expr: String,
    /// Lifecycle state.
    pub status: CronStatus,
    /// Delivery mode label.
    pub delivery: String,
    /// Optional mission tag.
    pub mission_tag: Option<String>,
    /// Optional channel binding.
    pub channel_binding: Option<String>,
    /// When the cron last ran, if ever.
    pub last_run_at: Option<DateTime<Utc>>,
    /// Success rate over the retained run history, `0.0..=1.0`.
    pub success_rate: f32,
    /// Average run duration in milliseconds over retained history.
    pub avg_duration_ms: u64,
    /// Total cost in cents over retained history.
    pub total_cost_cents: u64,
    /// Total fire count.
    pub run_count: u64,
}

/// A projected, redaction-safe detail view (the "drawer") for one cron.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CronDetail {
    /// The list-row projection.
    pub row: CronRow,
    /// Redacted prompt/mission text.
    pub prompt: String,
    /// Redacted delivery mode.
    pub delivery_mode: DeliveryMode,
    /// Model override, if any.
    pub model_override: Option<String>,
    /// Thinking override, if any.
    pub thinking_override: Option<String>,
    /// Redacted run history (most recent last).
    pub run_history: Vec<RunSummary>,
}

/// A mutation request against the cron store.
#[derive(Debug, Clone, PartialEq)]
pub enum CronMutation {
    /// Create a new cron owned by the operator.
    Create(CreateRequest),
    /// Pause an existing cron.
    Pause(String),
    /// Resume a paused cron.
    Resume(String),
    /// Delete an existing cron.
    Delete(String),
    /// Edit mutable fields of an existing cron.
    Edit {
        /// Target cron id.
        id: String,
        /// The fields to change.
        changes: EditChanges,
    },
}

impl CronMutation {
    /// The receipt verb for this mutation's *attempt* event.
    pub fn verb(&self) -> &'static str {
        match self {
            CronMutation::Create(_) => "cron.create.attempted.v1",
            CronMutation::Pause(_) => "cron.pause.attempted.v1",
            CronMutation::Resume(_) => "cron.resume.attempted.v1",
            CronMutation::Delete(_) => "cron.delete.attempted.v1",
            CronMutation::Edit { .. } => "cron.edit.attempted.v1",
        }
    }

    /// The receipt verb for this mutation's *success* event.
    pub fn success_verb(&self) -> &'static str {
        match self {
            CronMutation::Create(_) => "cron.create.succeeded.v1",
            CronMutation::Pause(_) => "cron.pause.succeeded.v1",
            CronMutation::Resume(_) => "cron.resume.succeeded.v1",
            CronMutation::Delete(_) => "cron.delete.succeeded.v1",
            CronMutation::Edit { .. } => "cron.edit.succeeded.v1",
        }
    }

    /// A short human label for the mutation.
    pub fn label(&self) -> &'static str {
        match self {
            CronMutation::Create(_) => "create",
            CronMutation::Pause(_) => "pause",
            CronMutation::Resume(_) => "resume",
            CronMutation::Delete(_) => "delete",
            CronMutation::Edit { .. } => "edit",
        }
    }
}

/// Fields required to create a new cron.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateRequest {
    /// Operator-facing name.
    pub name: String,
    /// 5-field cron expression.
    pub schedule_expr: String,
    /// The prompt/mission to dispatch.
    pub prompt: String,
    /// Delivery target.
    pub delivery_mode: DeliveryMode,
    /// Optional model override.
    pub model_override: Option<String>,
    /// Optional mission tag.
    pub mission_tag: Option<String>,
}

/// Mutable fields of an existing cron.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EditChanges {
    /// New cron expression.
    pub schedule_expr: Option<String>,
    /// New delivery mode.
    pub delivery_mode: Option<DeliveryMode>,
    /// New model override.
    pub model_override: Option<String>,
    /// New thinking override.
    pub thinking_override: Option<String>,
    /// New mission tag.
    pub mission_tag: Option<String>,
}

/// The outcome of an applied mutation, carrying the receipt evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct MutationReport {
    /// The cron id the mutation targeted (or created).
    pub cron_id: String,
    /// The mutation label.
    pub action: String,
    /// Whether the mutation succeeded.
    pub success: bool,
    /// Receipt id of the *success* receipt, when the action succeeded.
    pub receipt_id: Option<String>,
}
