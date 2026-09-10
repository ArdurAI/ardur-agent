//! Text rendering of the cron list + detail (§9.4).
//!
//! The operator "UI" surface in this cycle is command-driven text output (the
//! ratatui TUI is a later increment). All fields rendered here are already
//! redaction-safe: callers pass [`CronRow`]/[`CronDetail`] projected by the
//! controller, which runs the sentinel scan.

use crate::domain::{CronDetail, CronRow, Density};

/// Render a list of rows as an aligned text table honoring the density mode.
pub fn render_list(rows: &[CronRow], density: Density) -> String {
    if rows.is_empty() {
        return "no crons".to_string();
    }
    let mut out = String::new();
    match density {
        Density::Compact => {
            for r in rows {
                out.push_str(&format!("{} {}  {}\n", r.status.glyph(), r.id, r.name));
            }
        }
        Density::Default => {
            out.push_str("STATUS  ID                                    NAME                  SCHEDULE       DELIVERY  RUNS\n");
            for r in rows {
                out.push_str(&format!(
                    "{:<7} {:<37} {:<21} {:<14} {:<9} {}\n",
                    r.status.glyph(),
                    truncate(&r.id, 37),
                    truncate(&r.name, 21),
                    truncate(&r.schedule_expr, 14),
                    truncate(&r.delivery, 9),
                    r.run_count,
                ));
            }
        }
        Density::Comfortable => {
            for r in rows {
                out.push_str(&format!(
                    "{} {}  {}\n    schedule: {}   delivery: {}   runs: {}   success: {:.0}%   avg: {}ms   cost: {}c\n",
                    r.status.glyph(),
                    r.id,
                    r.name,
                    r.schedule_expr,
                    r.delivery,
                    r.run_count,
                    r.success_rate * 100.0,
                    r.avg_duration_ms,
                    r.total_cost_cents,
                ));
            }
        }
    }
    out.trim_end().to_string()
}

/// Render a per-cron detail "drawer" as text.
pub fn render_detail(detail: &CronDetail) -> String {
    let r = &detail.row;
    let mut out = String::new();
    out.push_str(&format!("{} {}  {}\n", r.status.glyph(), r.id, r.name));
    out.push_str(&format!("  schedule:  {}\n", r.schedule_expr));
    out.push_str(&format!("  delivery:  {}\n", detail.delivery_mode.label()));
    if let Some(m) = &detail.model_override {
        out.push_str(&format!("  model:     {m}\n"));
    }
    if let Some(t) = &detail.thinking_override {
        out.push_str(&format!("  thinking:  {t}\n"));
    }
    if let Some(tag) = &r.mission_tag {
        out.push_str(&format!("  tag:       {tag}\n"));
    }
    out.push_str(&format!("  prompt:    {}\n", detail.prompt));
    out.push_str(&format!(
        "  stats:     runs={} success={:.0}% avg={}ms cost={}c\n",
        r.run_count,
        r.success_rate * 100.0,
        r.avg_duration_ms,
        r.total_cost_cents
    ));
    if detail.run_history.is_empty() {
        out.push_str("  history:   (none)\n");
    } else {
        out.push_str("  history:\n");
        for run in detail.run_history.iter().rev().take(10) {
            let status = match &run.status {
                crate::domain::RunStatus::Success => "ok".to_string(),
                crate::domain::RunStatus::Failed { reason } => format!("failed: {reason}"),
                crate::domain::RunStatus::Skipped => "skipped".to_string(),
            };
            out.push_str(&format!(
                "    {}  {}ms  {}c  {}\n",
                run.started_at.format("%Y-%m-%d %H:%M:%SZ"),
                run.duration_ms,
                run.cost_cents,
                status,
            ));
        }
    }
    out.trim_end().to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max <= 1 {
        s.chars().take(max).collect()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}
