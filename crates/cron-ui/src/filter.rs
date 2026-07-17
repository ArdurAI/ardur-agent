//! Typed filter expressions for the cron list view (§9.4).
//!
//! Filter chips compile to a [`CronFilter`]; combination is AND (chips
//! intersect). Free-text search is sentinel-scanned before it is applied so a
//! pasted secret never becomes a query term.

use chrono::Utc;

use crate::domain::CronRow;
use crate::redaction::Redactor;

/// Which statuses a status chip admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusSet {
    /// Admit active crons.
    pub active: bool,
    /// Admit paused crons.
    pub paused: bool,
    /// Admit errored crons.
    pub errored: bool,
}

impl StatusSet {
    /// Whether the given status is admitted.
    pub fn admits(&self, status: crate::domain::CronStatus) -> bool {
        use crate::domain::CronStatus::*;
        match status {
            Active => self.active,
            Paused => self.paused,
            Errored => self.errored,
        }
    }
}

/// A typed filter expression. `Composite` combines sub-filters with AND.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum CronFilter {
    /// Admit everything (identity of the AND monoid).
    #[default]
    All,
    /// AND of sub-filters.
    Composite(Vec<CronFilter>),
    /// Admit only the given statuses.
    Status(StatusSet),
    /// Admit crons that last ran within the given number of seconds.
    LastRunWithinSecs(u64),
    /// Admit crons whose mission tag equals the given value.
    MissionTag(String),
    /// Admit crons whose channel binding equals the given value.
    ChannelBinding(String),
    /// Admit crons whose name contains the (sentinel-scanned) text.
    SearchText(String),
}

impl CronFilter {
    /// Whether a row satisfies the filter.
    pub fn matches(&self, row: &CronRow) -> bool {
        match self {
            CronFilter::All => true,
            CronFilter::Composite(parts) => parts.iter().all(|p| p.matches(row)),
            CronFilter::Status(set) => set.admits(row.status),
            CronFilter::LastRunWithinSecs(secs) => match row.last_run_at {
                Some(ts) => {
                    let age = Utc::now().signed_duration_since(ts).num_seconds();
                    age >= 0 && (age as u64) <= *secs
                }
                None => false,
            },
            CronFilter::MissionTag(tag) => row.mission_tag.as_deref() == Some(tag.as_str()),
            CronFilter::ChannelBinding(ch) => row.channel_binding.as_deref() == Some(ch.as_str()),
            CronFilter::SearchText(needle) => {
                let hay = row.name.to_lowercase();
                hay.contains(&needle.to_lowercase())
            }
        }
    }

    /// Parse a single-chip filter expression. Recognised forms:
    ///
    /// - `status:active`, `status:paused`, `status:errored`, `status:active,errored`
    /// - `tag:<mission-tag>`
    /// - `channel:<binding>`
    /// - `last-run:<seconds>`
    /// - anything else → a sentinel-scanned free-text search over the name.
    ///
    /// The `redactor` scrubs free-text before it becomes a query term so a
    /// pasted credential never lands in the filter (or in a later receipt).
    pub fn parse(expr: &str, redactor: &Redactor) -> CronFilter {
        let expr = expr.trim();
        if let Some(rest) = expr.strip_prefix("status:") {
            let mut set = StatusSet {
                active: false,
                paused: false,
                errored: false,
            };
            for part in rest.split(',') {
                match part.trim() {
                    "active" => set.active = true,
                    "paused" => set.paused = true,
                    "errored" => set.errored = true,
                    _ => {}
                }
            }
            return CronFilter::Status(set);
        }
        if let Some(tag) = expr.strip_prefix("tag:") {
            return CronFilter::MissionTag(tag.trim().to_string());
        }
        if let Some(ch) = expr.strip_prefix("channel:") {
            return CronFilter::ChannelBinding(ch.trim().to_string());
        }
        if let Some(secs) = expr.strip_prefix("last-run:") {
            if let Ok(n) = secs.trim().parse::<u64>() {
                return CronFilter::LastRunWithinSecs(n);
            }
        }
        // Free text: sentinel-scan before it becomes a query term.
        CronFilter::SearchText(redactor.scan(expr).into_owned())
    }
}
