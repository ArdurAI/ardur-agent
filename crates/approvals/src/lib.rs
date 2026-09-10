//! The shared on-disk approval-card store.
//!
//! PR #279 (`feat/approvals-endpoints-2026-07-12`) mounted the **decide**
//! half of the approval-gate loop — `GET /approvals`, `POST
//! /approvals/{id}/approve`, `POST /approvals/{id}/reject` — over a shared
//! filesystem store at `<data_dir>/approvals/<id>.json`, the same directory
//! the CLI's `ardur approvals` subcommand already read/wrote as loose
//! `serde_json::Value`. Nothing produced a pending card.
//!
//! This crate is the **propose** half's shared substrate: a typed
//! [`ApprovalCard`]/[`ApprovalStatus`] and an [`ApprovalStore`] that knows how
//! to create, find, list, and decide cards against that same directory —
//! consolidating what were three independent hand-rolled implementations
//! (the CLI's `run_approvals`, the server's `apply_approval_decision`, and
//! now the runtime's propose path) onto one piece of logic, so the id
//! validation, atomic-write, and 404/409 decide semantics stay identical
//! everywhere a card is touched.
//!
//! The on-disk schema stays intentionally open (PR #279's own `openapi.rs`
//! documents "additional producer-defined fields may be present") —
//! [`ApprovalCard`] adds `tool`, `capability`, `arguments_digest`,
//! `session_id`, and `reason` on top of the original `id`/`status`/
//! `decided_at`/`deny_reason` fields, all `#[serde(skip_serializing_if =
//! "Option::is_none")]` so an older reader that only knows the original
//! fields still parses a card written by this crate, and a card written
//! before this crate existed (no propose-specific fields) still parses here.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// `<approvals_dir>/<id>.json` ids are restricted to this alphabet and
/// length — identical to PR #279's `valid_approval_id`/`MAX_APPROVAL_ID_LEN`
/// in `crates/server/src/routes.rs`, duplicated here (rather than an
/// inter-crate dependency on `ardur-server`, which would invert the
/// dependency direction the server needs) so every producer and consumer of
/// the store enforces the exact same traversal-safety rule.
const MAX_APPROVAL_ID_LEN: usize = 128;

/// Whether `id` is safe to join onto the approvals directory: non-empty, at
/// most [`MAX_APPROVAL_ID_LEN`] bytes, and drawn only from
/// `[A-Za-z0-9_-]` — no `.`/`/`/NUL, so it can never traverse out of the
/// directory once joined onto a path.
#[must_use]
pub fn valid_approval_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_APPROVAL_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// An approval card's lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalStatus {
    /// Proposed, awaiting an operator decision.
    Pending,
    /// An operator approved it.
    Approved,
    /// An operator rejected it (the wire/CLI verb is `reject`/`deny`; the
    /// stored status, matching PR #279's own reconciliation, is `denied`).
    Denied,
}

impl ApprovalStatus {
    #[must_use]
    pub fn is_decided(self) -> bool {
        !matches!(self, ApprovalStatus::Pending)
    }
}

/// One approval card, as persisted at `<approvals_dir>/<id>.json`.
///
/// `id` is not itself a field — it is the filename (minus `.json`), injected
/// by the reader after a lookup, matching PR #279's own convention ("each
/// with its `id` injected").
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalCard {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub status: ApprovalStatus,
    /// Unix seconds this card was proposed.
    pub created_at: u64,
    /// Unix seconds an operator decided this card, once decided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<u64>,
    /// The reason given for a `Denied` decision (empty string, not absent,
    /// for a reject with no reason — matches PR #279's existing convention
    /// so the wire shape does not change for the decide-half's consumers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deny_reason: Option<String>,
    /// The tool the gated call targeted.
    pub tool: String,
    /// The capability that triggered approval-gating for this call.
    pub capability: String,
    /// `sha256(arguments)` hex digest of the tool call this card gates —
    /// the matching key a retried call is looked up by, so re-submitting
    /// the *same* call after approval proceeds without minting a second
    /// card, and a *different* call against the same tool proposes its own.
    pub arguments_digest: String,
    /// The session the gated call was made in, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// A short human-readable summary of why approval was requested (e.g.
    /// "tool `shell.run` requires capability `shell.exec`, which is
    /// approval-gated").
    pub reason: String,
}

/// A decision an operator can make against a pending card.
#[derive(Clone, Debug)]
pub enum Decision {
    Approve,
    Reject { reason: String },
}

/// A failure reading, writing, or deciding an approval card.
#[derive(Debug, thiserror::Error)]
pub enum ApprovalStoreError {
    #[error("invalid approval id")]
    InvalidId,
    #[error("approval card not found")]
    NotFound,
    #[error("approval card already decided")]
    AlreadyDecided,
    #[error("approval card is corrupt: {0}")]
    Corrupt(String),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// The shared on-disk approval-card store: `<data_dir>/approvals/*.json`.
///
/// Every write goes through the same atomic temp-file-plus-`fsync`-plus-
/// `rename` sequence PR #279's `write_atomically` established, so a reader
/// (the CLI, the HTTP `GET /approvals` handler, or another `ApprovalStore`
/// instance) never observes a torn record.
#[derive(Clone, Debug)]
pub struct ApprovalStore {
    dir: PathBuf,
}

impl ApprovalStore {
    /// Open a store rooted at `dir` (typically `<data_dir>/approvals`).
    /// Does not create the directory — callers that propose or decide a
    /// card create it lazily via [`ensure_dir`](Self::ensure_dir).
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory this store is rooted at.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn ensure_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)
    }

    fn path_for(&self, id: &str) -> Result<PathBuf, ApprovalStoreError> {
        if !valid_approval_id(id) {
            return Err(ApprovalStoreError::InvalidId);
        }
        Ok(self.dir.join(format!("{id}.json")))
    }

    /// Read one card by id, with its `id` injected.
    ///
    /// # Errors
    /// [`ApprovalStoreError::InvalidId`] for a malformed id,
    /// [`ApprovalStoreError::NotFound`] if no card exists,
    /// [`ApprovalStoreError::Corrupt`] if the file is not valid JSON.
    pub fn read(&self, id: &str) -> Result<ApprovalCard, ApprovalStoreError> {
        let path = self.path_for(id)?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ApprovalStoreError::NotFound);
            }
            Err(e) => return Err(ApprovalStoreError::Io(e)),
        };
        let mut card: ApprovalCard = serde_json::from_slice(&bytes)
            .map_err(|e| ApprovalStoreError::Corrupt(e.to_string()))?;
        card.id = Some(id.to_string());
        Ok(card)
    }

    /// Every card in the store, each with its `id` injected. Skips (rather
    /// than fails on) a non-`.json` entry or a corrupt card, matching PR
    /// #279's list handler, which surfaces best-effort — a single bad file
    /// should not blank the whole list.
    pub fn list(&self) -> std::io::Result<Vec<ApprovalCard>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut cards = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Ok(card) = self.read(id) {
                cards.push(card);
            }
        }
        Ok(cards)
    }

    /// Find a pending or already-decided card matching `(tool,
    /// arguments_digest)` exactly, and `session_id` when the candidate has
    /// one recorded. Used by the propose path to make re-checking the same
    /// gated call idempotent: a second identical call while a card is still
    /// pending returns the *same* card rather than minting a duplicate, and
    /// a call after approval finds the approved card rather than proposing
    /// again.
    pub fn find_matching(
        &self,
        tool: &str,
        arguments_digest: &str,
        session_id: Option<&str>,
    ) -> std::io::Result<Option<ApprovalCard>> {
        let cards = self.list()?;
        Ok(cards.into_iter().find(|c| {
            c.tool == tool
                && c.arguments_digest == arguments_digest
                && c.session_id.as_deref() == session_id
        }))
    }

    /// Create a new `Pending` card with a fresh id and write it atomically.
    /// Returns the card with its `id` injected.
    ///
    /// # Errors
    /// [`ApprovalStoreError::Io`] if the directory cannot be created or the
    /// card cannot be written.
    pub fn propose(
        &self,
        tool: impl Into<String>,
        capability: impl Into<String>,
        arguments_digest: impl Into<String>,
        session_id: Option<String>,
        reason: impl Into<String>,
        created_at: u64,
    ) -> Result<ApprovalCard, ApprovalStoreError> {
        self.ensure_dir()?;
        let id = uuid::Uuid::now_v7().to_string();
        let card = ApprovalCard {
            id: Some(id.clone()),
            status: ApprovalStatus::Pending,
            created_at,
            decided_at: None,
            deny_reason: None,
            tool: tool.into(),
            capability: capability.into(),
            arguments_digest: arguments_digest.into(),
            session_id,
            reason: reason.into(),
        };
        self.write_atomically(&id, &card)?;
        Ok(card)
    }

    /// Apply `decision` to the `Pending` card `id`, stamp `decided_at`, and
    /// write the result atomically. Returns the updated card.
    ///
    /// # Errors
    /// [`ApprovalStoreError::InvalidId`]/[`NotFound`](ApprovalStoreError::NotFound)/
    /// [`Corrupt`](ApprovalStoreError::Corrupt) as [`read`](Self::read).
    /// [`ApprovalStoreError::AlreadyDecided`] if `id`'s card is not
    /// currently `Pending` — repeated decide calls are idempotent-safe: the
    /// mutation fires exactly once.
    pub fn decide(
        &self,
        id: &str,
        decision: Decision,
        decided_at: u64,
    ) -> Result<ApprovalCard, ApprovalStoreError> {
        let mut card = self.read(id)?;
        if card.status.is_decided() {
            return Err(ApprovalStoreError::AlreadyDecided);
        }
        match decision {
            Decision::Approve => {
                card.status = ApprovalStatus::Approved;
            }
            Decision::Reject { reason } => {
                card.status = ApprovalStatus::Denied;
                card.deny_reason = Some(reason);
            }
        }
        card.decided_at = Some(decided_at);
        self.write_atomically(id, &card)?;
        Ok(card)
    }

    fn write_atomically(&self, id: &str, card: &ApprovalCard) -> Result<(), ApprovalStoreError> {
        self.ensure_dir()?;
        let path = self.path_for(id)?;
        let bytes = serde_json::to_vec_pretty(card)
            .map_err(|e| ApprovalStoreError::Corrupt(e.to_string()))?;
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        let tmp = dir.join(format!(".approval-{}.tmp", uuid::Uuid::now_v7().simple()));
        {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        match std::fs::rename(&tmp, &path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(ApprovalStoreError::Io(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, ApprovalStore) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ApprovalStore::new(dir.path().join("approvals"));
        (dir, store)
    }

    #[test]
    fn propose_writes_a_pending_card_with_an_injected_id() {
        let (_dir, store) = store();
        let card = store
            .propose(
                "shell.run",
                "shell.exec",
                "deadbeef",
                Some("sess-1".to_string()),
                "needs approval",
                1000,
            )
            .expect("propose succeeds");
        assert!(card.id.is_some());
        assert_eq!(card.status, ApprovalStatus::Pending);
        assert_eq!(card.tool, "shell.run");

        let read_back = store
            .read(card.id.as_deref().unwrap())
            .expect("read succeeds");
        assert_eq!(read_back.arguments_digest, "deadbeef");
    }

    #[test]
    fn find_matching_locates_the_right_card_and_ignores_others() {
        let (_dir, store) = store();
        let target = store
            .propose(
                "shell.run",
                "shell.exec",
                "aaa",
                Some("sess-1".to_string()),
                "r",
                1,
            )
            .unwrap();
        store
            .propose(
                "shell.run",
                "shell.exec",
                "bbb",
                Some("sess-1".to_string()),
                "r",
                1,
            )
            .unwrap();
        store
            .propose(
                "shell.run",
                "shell.exec",
                "aaa",
                Some("sess-2".to_string()),
                "r",
                1,
            )
            .unwrap();

        let found = store
            .find_matching("shell.run", "aaa", Some("sess-1"))
            .expect("find succeeds")
            .expect("a match exists");
        assert_eq!(found.id, target.id);
    }

    #[test]
    fn decide_approve_flips_status_and_stamps_decided_at() {
        let (_dir, store) = store();
        let card = store.propose("t", "c", "d", None, "r", 1).unwrap();
        let id = card.id.clone().unwrap();

        let decided = store
            .decide(&id, Decision::Approve, 42)
            .expect("decide succeeds");
        assert_eq!(decided.status, ApprovalStatus::Approved);
        assert_eq!(decided.decided_at, Some(42));
    }

    #[test]
    fn decide_reject_records_the_reason() {
        let (_dir, store) = store();
        let card = store.propose("t", "c", "d", None, "r", 1).unwrap();
        let id = card.id.clone().unwrap();

        let decided = store
            .decide(
                &id,
                Decision::Reject {
                    reason: "too risky".to_string(),
                },
                42,
            )
            .expect("decide succeeds");
        assert_eq!(decided.status, ApprovalStatus::Denied);
        assert_eq!(decided.deny_reason.as_deref(), Some("too risky"));
    }

    #[test]
    fn decide_twice_is_rejected_not_silently_overwritten() {
        let (_dir, store) = store();
        let card = store.propose("t", "c", "d", None, "r", 1).unwrap();
        let id = card.id.clone().unwrap();

        store.decide(&id, Decision::Approve, 1).unwrap();
        let second = store.decide(&id, Decision::Approve, 2);
        assert!(matches!(second, Err(ApprovalStoreError::AlreadyDecided)));
    }

    #[test]
    fn read_unknown_id_is_not_found() {
        let (_dir, store) = store();
        let err = store
            .read("00000000-0000-0000-0000-000000000000")
            .unwrap_err();
        assert!(matches!(err, ApprovalStoreError::NotFound));
    }

    #[test]
    fn traversal_shaped_ids_are_rejected() {
        let (_dir, store) = store();
        for bad in ["../etc/passwd", "a/b", "a.json", ""] {
            assert!(
                matches!(store.read(bad), Err(ApprovalStoreError::InvalidId)),
                "id {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn list_is_empty_for_a_missing_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = ApprovalStore::new(dir.path().join("does-not-exist"));
        assert!(store.list().expect("list succeeds").is_empty());
    }

    #[test]
    fn list_returns_every_proposed_card() {
        let (_dir, store) = store();
        store.propose("a", "c", "1", None, "r", 1).unwrap();
        store.propose("b", "c", "2", None, "r", 1).unwrap();
        assert_eq!(store.list().unwrap().len(), 2);
    }
}
