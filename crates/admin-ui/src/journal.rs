//! Read-only access to ardur-server's session journals.
//!
//! ardur-server's [`FileSessionJournal`](ardur_session_journals::FileSessionJournal)
//! persists one session per directory:
//! `<journal-dir>/sessions/<session-id>/journal.jsonl`, one serialized
//! [`JournalEntry`] per line. We read those files and never open them for write.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use ardur_session_journals::JournalEntry;
use serde::Serialize;

/// A one-line summary of a session, for the sessions list + dashboard table.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    /// The session id (the directory name).
    pub id: String,
    /// Journal-file mtime, milliseconds since the Unix epoch.
    pub modified_ms: u64,
    /// Number of user + assistant messages in the journal.
    pub message_count: usize,
    /// Total entries (including cost/checkpoint/invalidation records).
    pub entry_count: usize,
    /// Timestamp of the latest entry (`at`), milliseconds since epoch, if any.
    pub last_activity_ms: Option<u64>,
    /// Cents settled by the most recent `CostFinalized` entry, if any.
    pub last_cost_cents: Option<u64>,
}

/// A paginated window over one session's journal entries.
#[derive(Debug, Clone, Serialize)]
pub struct JournalPage {
    /// The session this page belongs to.
    pub session_id: String,
    /// Total entries in the journal.
    pub total: usize,
    /// 0-based index of the first returned entry within the full journal.
    pub offset: usize,
    /// The page-size requested.
    pub limit: usize,
    /// Number of entries actually returned.
    pub returned: usize,
    /// The entries, in append (chronological) order.
    pub entries: Vec<JournalEntry>,
}

/// The directory holding the per-session journal sub-directories.
fn sessions_root(journal_dir: &Path) -> PathBuf {
    journal_dir.join("sessions")
}

/// The journal file for one session id.
fn journal_path(journal_dir: &Path, session_id: &str) -> PathBuf {
    sessions_root(journal_dir)
        .join(session_id)
        .join("journal.jsonl")
}

/// The `at` (millisecond) timestamp an entry records.
fn entry_at(entry: &JournalEntry) -> u64 {
    match entry {
        JournalEntry::UserMessage { at, .. }
        | JournalEntry::AssistantMessage { at, .. }
        | JournalEntry::ToolInvocation { at, .. }
        | JournalEntry::CostFinalized { at, .. }
        | JournalEntry::Checkpoint { at, .. }
        | JournalEntry::Invalidation { at, .. } => *at,
    }
}

/// Whether an entry is a conversational message (user or assistant).
fn is_message(entry: &JournalEntry) -> bool {
    matches!(
        entry,
        JournalEntry::UserMessage { .. } | JournalEntry::AssistantMessage { .. }
    )
}

/// The cents an entry settled, if it is a `CostFinalized`.
fn cost_cents(entry: &JournalEntry) -> Option<u64> {
    match entry {
        JournalEntry::CostFinalized { actual, .. } => Some(actual.cents),
        _ => None,
    }
}

/// Parse every entry of one session's journal, in append order. Blank lines are
/// skipped; a malformed line aborts with the parse error (the file is corrupt).
pub fn read_entries(journal_dir: &Path, session_id: &str) -> anyhow::Result<Vec<JournalEntry>> {
    let path = journal_path(journal_dir, session_id);
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<JournalEntry>(l).map_err(anyhow::Error::from))
        .collect()
}

/// List the session ids present under the journal directory, newest journal
/// first (by file mtime). A missing or empty `sessions/` directory is an empty
/// list rather than an error.
pub fn list_sessions(journal_dir: &Path) -> anyhow::Result<Vec<SessionSummary>> {
    let root = sessions_root(journal_dir);
    let read_dir = match fs::read_dir(&root) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let mut out = Vec::new();
    for dirent in read_dir {
        let dirent = dirent?;
        if !dirent.file_type()?.is_dir() {
            continue;
        }
        let Some(id) = dirent.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let path = journal_path(journal_dir, &id);
        if !path.is_file() {
            continue;
        }
        let modified_ms = fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let entries = read_entries(journal_dir, &id)?;
        let message_count = entries.iter().filter(|e| is_message(e)).count();
        let last_activity_ms = entries.iter().map(entry_at).max();
        let last_cost_cents = entries.iter().rev().find_map(cost_cents);

        out.push(SessionSummary {
            id,
            modified_ms,
            message_count,
            entry_count: entries.len(),
            last_activity_ms,
            last_cost_cents,
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.modified_ms));
    Ok(out)
}

/// Build a page over one session's entries.
///
/// With no explicit `offset`, the page is the journal's **tail** — the last
/// `limit` entries — matching the dashboard's "recent activity" default. An
/// explicit `offset` pages forward from the start of the journal instead.
pub fn page(
    journal_dir: &Path,
    session_id: &str,
    limit: usize,
    offset: Option<usize>,
) -> anyhow::Result<JournalPage> {
    let entries = read_entries(journal_dir, session_id)?;
    let total = entries.len();
    let limit = limit.max(1);
    let start = match offset {
        Some(o) => o.min(total),
        None => total.saturating_sub(limit),
    };
    let end = start.saturating_add(limit).min(total);
    let window = entries[start..end].to_vec();
    Ok(JournalPage {
        session_id: session_id.to_string(),
        total,
        offset: start,
        limit,
        returned: window.len(),
        entries: window,
    })
}

/// Aggregate cents settled per session (summing every `CostFinalized`), for the
/// "top expensive sessions" cost view. Sessions with no settled cost are
/// omitted.
pub fn cents_by_session(journal_dir: &Path) -> anyhow::Result<Vec<(String, u64)>> {
    let mut out = Vec::new();
    for summary in list_sessions(journal_dir)? {
        let entries = read_entries(journal_dir, &summary.id)?;
        let cents: u64 = entries.iter().filter_map(cost_cents).sum();
        if cents > 0 {
            out.push((summary.id, cents));
        }
    }
    out.sort_by_key(|t| std::cmp::Reverse(t.1));
    Ok(out)
}
