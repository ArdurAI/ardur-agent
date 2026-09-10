//! Read-only access to ardur-server's session journals.
//!
//! ardur-server's [`FileSessionJournal`](ardur_session_journals::FileSessionJournal)
//! persists one session per directory:
//! `<journal-dir>/sessions/<session-id>/journal.jsonl`, one serialized
//! [`JournalEntry`] per line. We read those files and never open them for write.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use ardur_session_journals::{JournalEntry, redact_entries_default};
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

/// Whether `id` is safe to use as the session directory name: exactly one
/// `Normal` path component (no separators, no `.`/`..`, no root/prefix), so a
/// caller-supplied id can never escape the sessions root. Backslash is
/// rejected explicitly — it is a legal byte in Unix file names, so a single
/// `Normal` component can still smuggle a Windows separator.
pub fn is_valid_session_id(id: &str) -> bool {
    let mut components = Path::new(id).components();
    matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    ) && !id.contains('\\')
}

/// Resolve a caller-supplied session id to its journal file by matching it
/// against the directory entries actually present under the sessions root —
/// the returned path is built from the matched entry's own name, never by
/// joining the raw id. Combined with [`is_valid_session_id`], this confines
/// every journal read to `<journal-dir>/sessions/<existing-dir>/`. An id that
/// is invalid or names no existing session resolves to `None`.
fn resolve_journal_path(journal_dir: &Path, session_id: &str) -> Option<PathBuf> {
    if !is_valid_session_id(session_id) {
        return None;
    }
    let root = sessions_root(journal_dir);
    let read_dir = fs::read_dir(&root).ok()?;
    for dirent in read_dir.flatten() {
        if !dirent.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = dirent.file_name();
        if name.to_str() == Some(session_id) {
            return Some(root.join(name).join("journal.jsonl"));
        }
    }
    None
}

/// The `at` (millisecond) timestamp an entry records.
fn entry_at(entry: &JournalEntry) -> u64 {
    match entry {
        JournalEntry::UserMessage { at, .. }
        | JournalEntry::AssistantMessage { at, .. }
        | JournalEntry::ToolInvocation { at, .. }
        | JournalEntry::CostFinalized { at, .. }
        | JournalEntry::Checkpoint { at, .. }
        | JournalEntry::Invalidation { at, .. }
        | JournalEntry::Rollback { at, .. } => at.get(),
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
///
/// The session id is resolved via [`resolve_journal_path`] — validated and
/// matched against real directory entries before any file is opened — so an
/// id that would traverse outside `<journal-dir>/sessions/` (or names no
/// existing session) reads as "no such session" (empty).
pub fn read_entries(journal_dir: &Path, session_id: &str) -> anyhow::Result<Vec<JournalEntry>> {
    let Some(path) = resolve_journal_path(journal_dir, session_id) else {
        return Ok(Vec::new());
    };
    read_entries_at(&path)
}

/// Parse every entry of the journal file at `path` (a path already produced
/// by [`resolve_journal_path`] or built from a directory entry we enumerated).
fn read_entries_at(path: &Path) -> anyhow::Result<Vec<JournalEntry>> {
    let raw = match fs::read_to_string(path) {
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
        let name = dirent.file_name();
        let Some(id) = name.to_str().map(str::to_string) else {
            continue;
        };
        // Build the path from the enumerated directory entry itself — never
        // by joining a string id — so every read stays under `sessions/`.
        let path = root.join(&name).join("journal.jsonl");
        if !path.is_file() {
            continue;
        }
        let modified_ms = fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let entries = read_entries_at(&path)?;
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
///
/// Free-text fields (`UserMessage`/`AssistantMessage` content, `Checkpoint`
/// summaries, `Invalidation` reasons) are redacted for secret-shaped
/// patterns before the page is returned — this is the only place a
/// journal's raw content leaves the process, so redaction happens here
/// rather than at each caller.
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
    let window = redact_entries_default(&entries[start..end]);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_session_ids_pass() {
        for id in [
            "sess-a",
            "0192cafe-8a2b-7cde-9f01-234567890abc",
            "with.dot",
            "with_underscore",
        ] {
            assert!(is_valid_session_id(id), "{id} should be accepted");
        }
    }

    #[test]
    fn traversal_and_multi_component_ids_are_rejected() {
        for id in [
            "",
            ".",
            "..",
            "../outside",
            "a/b",
            "/etc/hostname",
            "..\\outside",
            "a\\b",
            "sessions/../../x",
        ] {
            assert!(!is_valid_session_id(id), "{id:?} must be rejected");
        }
    }

    #[test]
    fn read_entries_confines_ids_to_sessions_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal_dir = dir.path().join("journals");

        // A journal OUTSIDE the sessions root, reachable only by traversal.
        let outside = journal_dir.join("outside");
        fs::create_dir_all(&outside).expect("outside dir");
        fs::write(
            outside.join("journal.jsonl"),
            "{\"kind\":\"UserMessage\",\"content\":\"leaked\",\"at\":1}\n",
        )
        .expect("outside journal");

        // Traversal ids resolve to no session (empty), not the outside file.
        for id in ["../outside", "..", "outside/../outside"] {
            let entries = read_entries(&journal_dir, id).expect("read");
            assert!(entries.is_empty(), "{id:?} must not read outside sessions/");
        }

        // A legitimate session under sessions/ still reads fine.
        let inside = journal_dir.join("sessions").join("sess-ok");
        fs::create_dir_all(&inside).expect("inside dir");
        fs::write(
            inside.join("journal.jsonl"),
            "{\"kind\":\"UserMessage\",\"content\":\"hello\",\"at\":1}\n",
        )
        .expect("inside journal");
        let entries = read_entries(&journal_dir, "sess-ok").expect("read");
        assert_eq!(entries.len(), 1, "confined id reads its own journal");
    }
}
