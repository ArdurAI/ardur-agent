//! Cost aggregation over the receipt log and the session journals.
//!
//! Per-turn cost lives on each receipt's [`ReceiptBody::cost`]; the
//! by-day / by-provider / windowed roll-ups are computed from the receipt
//! chain. Per-session totals come from the journals' `CostFinalized` entries,
//! since a receipt body carries no session id.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::journal;
use crate::receipts::{self, LoadedReceipt};

/// Milliseconds in a 24-hour day.
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// Cost rolled up by a single provider (verb) key.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderCost {
    /// The provider (verb) label.
    pub provider: String,
    /// Total cents attributed to it.
    pub cents: u64,
    /// Number of receipts.
    pub count: usize,
}

/// Cost rolled up by calendar day (UTC).
#[derive(Debug, Clone, Serialize)]
pub struct DayCost {
    /// `YYYY-MM-DD` (UTC).
    pub day: String,
    /// Total cents on that day.
    pub cents: u64,
    /// Number of receipts on that day.
    pub count: usize,
}

/// Cents attributed to one session.
#[derive(Debug, Clone, Serialize)]
pub struct SessionCost {
    /// The session id.
    pub session_id: String,
    /// Total cents settled in its journal.
    pub cents: u64,
}

/// The full cost report served at `/api/costs`.
#[derive(Debug, Clone, Serialize)]
pub struct CostsReport {
    /// Total cents across all receipts.
    pub total_cents: u64,
    /// Cents from receipts issued today (UTC), relative to `now`.
    pub today_cents: u64,
    /// Cents from receipts in the last 7 days.
    pub last_7d_cents: u64,
    /// Cents from receipts in the last 30 days.
    pub last_30d_cents: u64,
    /// Cents by provider (verb), highest first.
    pub by_provider: Vec<ProviderCost>,
    /// Cents by calendar day (UTC), most recent day first.
    pub by_day: Vec<DayCost>,
    /// Top 10 most expensive sessions (by settled cents).
    pub top_sessions: Vec<SessionCost>,
}

/// Format a millisecond epoch as a `YYYY-MM-DD` UTC day, falling back to the
/// raw value's day-bucket string if it is out of `chrono`'s representable range.
fn day_of(ms: u64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms as i64)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| format!("day-{}", ms / DAY_MS))
}

/// Compute the receipt-derived portions of the report (everything except
/// `top_sessions`). Split out so it is unit-testable without a clock or
/// journals — `now_ms` is injected.
pub fn aggregate_receipts(receipts: &[LoadedReceipt], now_ms: u64) -> CostsReport {
    let mut total = 0u64;
    let mut today = 0u64;
    let mut last_7d = 0u64;
    let mut last_30d = 0u64;
    let mut by_provider: BTreeMap<String, (u64, usize)> = BTreeMap::new();
    let mut by_day: BTreeMap<String, (u64, usize)> = BTreeMap::new();

    let today_key = day_of(now_ms);

    for r in receipts {
        let cents = r.body.cost.cents;
        let issued = r.body.issued_at.0;
        total = total.saturating_add(cents);

        let day = day_of(issued);
        if day == today_key {
            today = today.saturating_add(cents);
        }
        // Windows are inclusive of `now`; a receipt stamped in the future
        // (clock skew) still counts toward the most recent window.
        let age = now_ms.saturating_sub(issued.min(now_ms));
        if age <= 7 * DAY_MS {
            last_7d = last_7d.saturating_add(cents);
        }
        if age <= 30 * DAY_MS {
            last_30d = last_30d.saturating_add(cents);
        }

        let p = by_provider.entry(r.provider().to_string()).or_default();
        p.0 = p.0.saturating_add(cents);
        p.1 += 1;

        let d = by_day.entry(day).or_default();
        d.0 = d.0.saturating_add(cents);
        d.1 += 1;
    }

    let mut by_provider: Vec<ProviderCost> = by_provider
        .into_iter()
        .map(|(provider, (cents, count))| ProviderCost {
            provider,
            cents,
            count,
        })
        .collect();
    by_provider.sort_by(|a, b| {
        b.cents
            .cmp(&a.cents)
            .then_with(|| a.provider.cmp(&b.provider))
    });

    // Days ascending in the map; present most-recent first.
    let mut by_day: Vec<DayCost> = by_day
        .into_iter()
        .map(|(day, (cents, count))| DayCost { day, cents, count })
        .collect();
    by_day.sort_by(|a, b| b.day.cmp(&a.day));

    CostsReport {
        total_cents: total,
        today_cents: today,
        last_7d_cents: last_7d,
        last_30d_cents: last_30d,
        by_provider,
        by_day,
        top_sessions: Vec::new(),
    }
}

/// Build the full report: receipt roll-ups + the top-10 sessions by settled
/// cents from the journals.
pub fn report(
    receipt_store: &Path,
    journal_dir: &Path,
    now_ms: u64,
) -> anyhow::Result<CostsReport> {
    let chain = receipts::load_chain(receipt_store)?;
    let mut report = aggregate_receipts(&chain, now_ms);

    let mut sessions: Vec<SessionCost> = journal::cents_by_session(journal_dir)?
        .into_iter()
        .map(|(session_id, cents)| SessionCost { session_id, cents })
        .collect();
    sessions.truncate(10);
    report.top_sessions = sessions;

    Ok(report)
}
