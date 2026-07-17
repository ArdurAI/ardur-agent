//! The cron operator controller (§9.4).
//!
//! A stateless inspector/controller over the durable cron store. It enforces
//! cap-token-scoped visibility, sentinel-redacts every rendered field, and
//! emits a receipt for every action (view + mutate). Mutations are refused
//! unless the operator holds [`SCOPE_MUTATE`](crate::gate::SCOPE_MUTATE).

use chrono::Utc;

use crate::domain::{
    CreateRequest, CronDetail, CronMutation, CronRecord, CronRow, CronStatus, DeliveryMode,
    EditChanges, MutationReport, VisibilityTier,
};
use crate::error::{CronUiError, Result};
use crate::filter::CronFilter;
use crate::gate::{Principal, ReceiptEvent, ReceiptSink, SCOPE_ADMIN, SCOPE_MUTATE, SCOPE_PROJECT};
use crate::redaction::Redactor;
use crate::store::CronStore;

/// Validate a 5-field cron expression against the field grammar the
/// `ardur-cron` matcher supports (`*`, integers, `a-b` ranges, `a,b` lists,
/// `*/n` steps). Returns the normalized expression on success.
pub fn validate_cron(expr: &str) -> Result<String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    if fields.len() != 5 {
        return Err(CronUiError::InvalidCron(format!(
            "expected 5 fields, got {}",
            fields.len()
        )));
    }
    for field in &fields {
        if field.is_empty()
            || !field
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '*' | '-' | ',' | '/'))
        {
            return Err(CronUiError::InvalidCron(format!(
                "unsupported field `{field}`"
            )));
        }
    }
    Ok(fields.join(" "))
}

/// The cron operator controller.
pub struct CronController<S: CronStore, R: ReceiptSink> {
    store: S,
    receipts: R,
    redactor: Redactor,
}

impl<S: CronStore, R: ReceiptSink> CronController<S, R> {
    /// Build a controller over a store and a receipt sink.
    pub fn new(store: S, receipts: R) -> Self {
        Self {
            store,
            receipts,
            redactor: Redactor::new(),
        }
    }

    /// Borrow the underlying store (for wiring / tests).
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Borrow the underlying receipt sink (for wiring / tests).
    pub fn store_receipts(&self) -> &R {
        &self.receipts
    }

    /// Whether `principal` may see a record under `visibility`.
    fn visible(
        &self,
        principal: &Principal,
        record: &CronRecord,
        visibility: VisibilityTier,
    ) -> Result<bool> {
        match visibility {
            VisibilityTier::SelfOnly => Ok(record.owner_fingerprint == principal.fingerprint),
            VisibilityTier::Project => {
                principal.require(SCOPE_PROJECT)?;
                Ok(true)
            }
            VisibilityTier::Tenant => {
                principal.require(SCOPE_ADMIN)?;
                Ok(true)
            }
        }
    }

    /// Project a stored record into a redaction-safe list row with computed
    /// statistics.
    fn to_row(&self, record: &CronRecord) -> CronRow {
        let n = record.run_history.len();
        let (successes, total_ms, total_cost) =
            record
                .run_history
                .iter()
                .fold((0u64, 0u64, 0u64), |(s, d, c), run| {
                    let ok = matches!(run.status, crate::domain::RunStatus::Success);
                    (s + u64::from(ok), d + run.duration_ms, c + run.cost_cents)
                });
        let success_rate = if n == 0 {
            0.0
        } else {
            successes as f32 / n as f32
        };
        let avg_duration_ms = if n == 0 { 0 } else { total_ms / n as u64 };
        CronRow {
            id: record.id.clone(),
            name: self.redactor.scan(&record.name).into_owned(),
            schedule_expr: record.schedule_expr.clone(),
            status: record.status,
            delivery: record.delivery_mode.label().to_string(),
            mission_tag: record.mission_tag.clone(),
            channel_binding: record.channel_binding.clone(),
            last_run_at: record.last_run().map(|r| r.started_at),
            success_rate,
            avg_duration_ms,
            total_cost_cents: total_cost,
            run_count: record.run_count,
        }
    }

    /// Redact a delivery mode's sensitive fields before render.
    fn redact_delivery(&self, mode: &DeliveryMode) -> DeliveryMode {
        match mode {
            DeliveryMode::Webhook { url } => DeliveryMode::Webhook {
                url: self.redactor.scan(url).into_owned(),
            },
            other => other.clone(),
        }
    }

    /// List crons visible to `principal`, filtered and redacted. Emits a
    /// `cron.ui.viewed.v1` receipt for the inspection.
    pub fn list(
        &self,
        principal: &Principal,
        filter: &CronFilter,
        visibility: VisibilityTier,
        now_unix_millis: u64,
    ) -> Result<Vec<CronRow>> {
        let mut rows = Vec::new();
        for record in self.store.load_all()? {
            if self.visible(principal, &record, visibility)? {
                let row = self.to_row(&record);
                if filter.matches(&row) {
                    rows.push(row);
                }
            }
        }
        // Deterministic ordering: by next-nothing available, so by name then id.
        rows.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        let payload = format!("list:{}:{}", rows.len(), now_unix_millis);
        self.receipts.emit(ReceiptEvent {
            verb: "cron.ui.viewed.v1",
            subject: &principal.subject,
            token_id: &principal.token_id,
            payload: payload.as_bytes(),
        })?;
        Ok(rows)
    }

    /// Fetch a redaction-safe detail view for one cron. Emits a
    /// `cron.ui.viewed.v1` receipt.
    pub fn detail(&self, principal: &Principal, id: &str) -> Result<CronDetail> {
        let record = self.store.get(id)?;
        if !self.visible(principal, &record, VisibilityTier::SelfOnly)?
            && !principal.has(SCOPE_ADMIN)
        {
            return Err(CronUiError::Denied(format!(
                "cron `{id}` not owned by operator"
            )));
        }
        let row = self.to_row(&record);
        let run_history = record
            .run_history
            .iter()
            .map(|run| {
                let mut run = run.clone();
                if let crate::domain::RunStatus::Failed { reason } = &run.status {
                    run.status = crate::domain::RunStatus::Failed {
                        reason: self.redactor.scan(reason).into_owned(),
                    };
                }
                run
            })
            .collect();
        let detail = CronDetail {
            row,
            prompt: self.redactor.scan(&record.prompt).into_owned(),
            delivery_mode: self.redact_delivery(&record.delivery_mode),
            model_override: record.model_override.clone(),
            thinking_override: record.thinking_override.clone(),
            run_history,
        };
        self.receipts.emit(ReceiptEvent {
            verb: "cron.ui.viewed.v1",
            subject: &principal.subject,
            token_id: &principal.token_id,
            payload: id.as_bytes(),
        })?;
        Ok(detail)
    }

    /// Apply a mutation. Emits an attempted receipt, then either a refused
    /// receipt (insufficient scope) or a success receipt.
    pub fn mutate(&self, principal: &Principal, mutation: CronMutation) -> Result<MutationReport> {
        let target = mutation_target(&mutation);
        // Every attempt is receipted regardless of outcome (ADR-Phase3-280).
        self.receipts.emit(ReceiptEvent {
            verb: mutation.verb(),
            subject: &principal.subject,
            token_id: &principal.token_id,
            payload: target.as_bytes(),
        })?;

        if !principal.has(SCOPE_MUTATE) {
            self.receipts.emit(ReceiptEvent {
                verb: "cron.mutate.refused.v1",
                subject: &principal.subject,
                token_id: &principal.token_id,
                payload: target.as_bytes(),
            })?;
            return Err(CronUiError::Denied(format!(
                "mutation `{}` requires scope `{SCOPE_MUTATE}`",
                mutation.label()
            )));
        }

        let (cron_id, success_verb, success_label) = match &mutation {
            CronMutation::Create(req) => {
                let id = self.apply_create(principal, req)?;
                (id, mutation.success_verb(), mutation.label())
            }
            CronMutation::Pause(id) => {
                self.set_status(principal, id, CronStatus::Paused)?;
                (id.clone(), mutation.success_verb(), mutation.label())
            }
            CronMutation::Resume(id) => {
                self.set_status(principal, id, CronStatus::Active)?;
                (id.clone(), mutation.success_verb(), mutation.label())
            }
            CronMutation::Delete(id) => {
                self.owned(principal, id)?;
                self.store.remove(id)?;
                (id.clone(), mutation.success_verb(), mutation.label())
            }
            CronMutation::Edit { id, changes } => {
                self.apply_edit(principal, id, changes)?;
                (id.clone(), mutation.success_verb(), mutation.label())
            }
        };

        let receipt_id = self.receipts.emit(ReceiptEvent {
            verb: success_verb,
            subject: &principal.subject,
            token_id: &principal.token_id,
            payload: cron_id.as_bytes(),
        })?;

        Ok(MutationReport {
            cron_id,
            action: success_label.to_string(),
            success: true,
            receipt_id: Some(receipt_id),
        })
    }

    /// Confirm the operator owns the cron (or holds admin scope).
    fn owned(&self, principal: &Principal, id: &str) -> Result<CronRecord> {
        let record = self.store.get(id)?;
        if record.owner_fingerprint != principal.fingerprint && !principal.has(SCOPE_ADMIN) {
            return Err(CronUiError::Denied(format!(
                "cron `{id}` not owned by operator"
            )));
        }
        Ok(record)
    }

    fn apply_create(&self, principal: &Principal, req: &CreateRequest) -> Result<String> {
        let schedule_expr = validate_cron(&req.schedule_expr)?;
        let now = Utc::now();
        let id = uuid::Uuid::now_v7().to_string();
        let record = CronRecord {
            id: id.clone(),
            name: req.name.clone(),
            schedule_expr,
            prompt: req.prompt.clone(),
            status: CronStatus::Active,
            owner_fingerprint: principal.fingerprint.clone(),
            delivery_mode: req.delivery_mode.clone(),
            model_override: req.model_override.clone(),
            thinking_override: None,
            mission_tag: req.mission_tag.clone(),
            channel_binding: match &req.delivery_mode {
                DeliveryMode::ChannelPeer { channel, .. } => Some(channel.clone()),
                _ => None,
            },
            created_at: now,
            updated_at: now,
            run_history: Vec::new(),
            run_count: 0,
        };
        self.store.upsert(record)?;
        Ok(id)
    }

    fn set_status(&self, principal: &Principal, id: &str, status: CronStatus) -> Result<()> {
        let mut record = self.owned(principal, id)?;
        record.status = status;
        record.updated_at = Utc::now();
        self.store.upsert(record)
    }

    fn apply_edit(&self, principal: &Principal, id: &str, changes: &EditChanges) -> Result<()> {
        let mut record = self.owned(principal, id)?;
        if let Some(expr) = &changes.schedule_expr {
            record.schedule_expr = validate_cron(expr)?;
        }
        if let Some(mode) = &changes.delivery_mode {
            record.delivery_mode = mode.clone();
            record.channel_binding = match mode {
                DeliveryMode::ChannelPeer { channel, .. } => Some(channel.clone()),
                _ => None,
            };
        }
        if changes.model_override.is_some() {
            record.model_override = changes.model_override.clone();
        }
        if changes.thinking_override.is_some() {
            record.thinking_override = changes.thinking_override.clone();
        }
        if changes.mission_tag.is_some() {
            record.mission_tag = changes.mission_tag.clone();
        }
        record.updated_at = Utc::now();
        self.store.upsert(record)
    }
}

fn mutation_target(mutation: &CronMutation) -> String {
    match mutation {
        CronMutation::Create(req) => req.name.clone(),
        CronMutation::Pause(id)
        | CronMutation::Resume(id)
        | CronMutation::Delete(id)
        | CronMutation::Edit { id, .. } => id.clone(),
    }
}
