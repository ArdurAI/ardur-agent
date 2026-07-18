//! A local, bounded advisory/vulnerability database check for marketplace
//! installs.
//!
//! This is **not** a live OSV/NVD/RustSec feed — no network call, no
//! background sync job, nothing that could hang an install on a flaky
//! connection or trust an unauthenticated remote source. It is a curated,
//! operator-supplied (or, in a future iteration, operator-synced-offline)
//! JSON file of `(skill_id, affected_versions, severity, summary)` entries,
//! checked locally at install/update time. The built-in default is an empty
//! database — this crate has no real-world CVE data to embed, and shipping
//! a fabricated one would be worse than shipping none — so this feature is
//! inert until an operator supplies `--advisory-db`/`ARDUR_MARKETPLACE_ADVISORY_DB`.
//! What's real here is the checking *mechanism*: bounded parsing, exact
//! version matching, severity-aware refusal with an explicit per-advisory
//! override, and `audit` visibility regardless of override.

use std::path::Path;

use ardur_cli::CliError;

/// Maximum entries a loaded advisory database may declare.
const MAX_ADVISORY_ENTRIES: usize = 10_000;
/// Advisory-database file byte-size ceiling (5 MiB).
const MAX_ADVISORY_FILE_BYTES: u64 = 5 * 1024 * 1024;
/// Maximum characters in an advisory's human-readable summary.
const MAX_SUMMARY_LEN: usize = 500;

/// An advisory's severity. Ordered `Info < Low < Medium < High < Critical` so
/// a `--max-severity` ceiling can compare.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Info => "info",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for Severity {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "info" => Ok(Self::Info),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "critical" => Ok(Self::Critical),
            other => Err(format!(
                "unknown severity `{other}` (expected info, low, medium, high, or critical)"
            )),
        }
    }
}

/// A single known-vulnerable `(skill_id, version)` record.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct AdvisoryEntry {
    pub advisory_id: String,
    pub skill_id: String,
    /// Exact version strings this advisory applies to. No range syntax — an
    /// operator lists the specific affected versions, keeping the format
    /// (and its parser) trivial and auditable.
    pub affected_versions: Vec<String>,
    pub severity: Severity,
    pub summary: String,
}

/// A loaded, bounded advisory database.
#[derive(Debug, Default)]
pub(crate) struct AdvisoryDatabase {
    entries: Vec<AdvisoryEntry>,
}

impl AdvisoryDatabase {
    /// The built-in default (empty) composed with `extra` (an
    /// operator-supplied JSON file), when given.
    pub(crate) fn load(extra: Option<&Path>) -> Result<Self, CliError> {
        let Some(path) = extra else {
            return Ok(Self::default());
        };
        let meta = std::fs::metadata(path)?;
        if meta.len() > MAX_ADVISORY_FILE_BYTES {
            return Err(CliError::State(format!(
                "advisory database {} is {} bytes, exceeding the {MAX_ADVISORY_FILE_BYTES}-byte ceiling",
                path.display(),
                meta.len()
            )));
        }
        let content = std::fs::read_to_string(path)?;
        let entries: Vec<AdvisoryEntry> = serde_json::from_str(&content).map_err(|e| {
            CliError::State(format!("invalid advisory database {}: {e}", path.display()))
        })?;
        if entries.len() > MAX_ADVISORY_ENTRIES {
            return Err(CliError::State(format!(
                "advisory database declares {} entries, exceeding the {MAX_ADVISORY_ENTRIES} ceiling",
                entries.len()
            )));
        }
        for entry in &entries {
            if entry.summary.chars().count() > MAX_SUMMARY_LEN {
                return Err(CliError::State(format!(
                    "advisory `{}` summary exceeds {MAX_SUMMARY_LEN} characters",
                    entry.advisory_id
                )));
            }
            if entry.advisory_id.trim().is_empty() || entry.skill_id.trim().is_empty() {
                return Err(CliError::State(
                    "advisory entries require non-empty advisory_id and skill_id".to_string(),
                ));
            }
        }
        Ok(Self { entries })
    }

    /// Every advisory matching `skill_id`'s exact `version`, in file order.
    pub(crate) fn matches(&self, skill_id: &str, version: &str) -> Vec<&AdvisoryEntry> {
        self.entries
            .iter()
            .filter(|e| e.skill_id == skill_id && e.affected_versions.iter().any(|v| v == version))
            .collect()
    }
}
