//! Read-only access to ardur-server's redacted security-event log.
//!
//! ardur-server appends one JSON object per line to `security-events.jsonl` —
//! one per turn blocked by a security gate (injection, policy, cap-token, cost,
//! hook, tool). The lines are already redaction-safe at the writer: injection
//! events carry flag *categories* and *pattern ids* but never the matched text.
//! This module only reads and rolls them up; it never writes.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// One injection flag as persisted (matched text already stripped at the writer).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlagSummary {
    /// The pattern that fired.
    pub pattern_id: String,
    /// The injection class.
    pub category: String,
    /// Match confidence in `0.0..=1.0`.
    pub confidence: f32,
}

/// One durable, redacted security event, decoded from the log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityEvent {
    /// When the block occurred (ms since epoch).
    pub at_ms: u64,
    /// The gate that blocked the turn.
    pub gate: String,
    /// The decision (`"deny"`).
    #[serde(default)]
    pub decision: Option<String>,
    /// Injection-defense stage id, for injection events.
    #[serde(default)]
    pub filter_id: Option<String>,
    /// Engine/config-authored reason, for non-injection gates.
    #[serde(default)]
    pub reason: Option<String>,
    /// Injection flags (injection gate only).
    #[serde(default)]
    pub flags: Vec<FlagSummary>,
}

/// A count of events attributed to one gate.
#[derive(Debug, Clone, Serialize)]
pub struct GateCount {
    /// The gate label (`injection`, `policy`, …).
    pub gate: String,
    /// How many events landed on it.
    pub count: usize,
}

/// The Trust Center security-event view: totals by gate plus the most recent
/// events, split into the injection stream and the policy/other-gate stream.
#[derive(Debug, Clone, Serialize)]
pub struct SecurityEventView {
    /// Whether a security-event log path was configured at all.
    pub enabled: bool,
    /// Total events across every gate.
    pub total: usize,
    /// Per-gate counts, highest first.
    pub by_gate: Vec<GateCount>,
    /// The most recent injection-gate events, newest first.
    pub injection: Vec<SecurityEvent>,
    /// The most recent non-injection (policy/cap/cost/hook/tool) events, newest
    /// first — the policy-decision stream.
    pub decisions: Vec<SecurityEvent>,
}

impl SecurityEventView {
    /// The view when no log path was configured.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            total: 0,
            by_gate: Vec::new(),
            injection: Vec::new(),
            decisions: Vec::new(),
        }
    }
}

/// Load every event from the log (append order). A missing file is an empty log;
/// an undecodable line is skipped (logged) rather than failing the whole read,
/// so one torn trailing write can't blind the panel.
pub fn load(path: &Path) -> anyhow::Result<Vec<SecurityEvent>> {
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<SecurityEvent>(line) {
            Ok(event) => out.push(event),
            Err(e) => tracing::warn!(error = %e, "skipping undecodable security-event line"),
        }
    }
    Ok(out)
}

/// Build the Trust Center view from the log at `path`, capping each recent
/// stream at `limit`. `None` yields the disabled view.
pub fn view(path: Option<&Path>, limit: usize) -> anyhow::Result<SecurityEventView> {
    let Some(path) = path else {
        return Ok(SecurityEventView::disabled());
    };
    let events = load(path)?;
    let total = events.len();

    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for e in &events {
        *counts.entry(e.gate.clone()).or_default() += 1;
    }
    let mut by_gate: Vec<GateCount> = counts
        .into_iter()
        .map(|(gate, count)| GateCount { gate, count })
        .collect();
    by_gate.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.gate.cmp(&b.gate)));

    let mut injection: Vec<SecurityEvent> = events
        .iter()
        .filter(|e| e.gate == "injection")
        .cloned()
        .collect();
    injection.reverse();
    injection.truncate(limit);

    let mut decisions: Vec<SecurityEvent> = events
        .iter()
        .filter(|e| e.gate != "injection")
        .cloned()
        .collect();
    decisions.reverse();
    decisions.truncate(limit);

    Ok(SecurityEventView {
        enabled: true,
        total,
        by_gate,
        injection,
        decisions,
    })
}
