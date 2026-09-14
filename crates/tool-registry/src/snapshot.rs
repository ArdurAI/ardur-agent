//! **gh#413.** Content snapshots taken before a destructive file write.
//!
//! A tool that overwrites a file destroys whatever was there. The receipt
//! chain records *that* the write happened and the digest of what was written,
//! but not what was lost — so an audit can prove a file changed and still not
//! say what it changed from, and a mistaken write has no undo.
//!
//! This module captures the prior bytes into a content-addressed store before
//! the write lands, and hands back a [`SnapshotId`] naming them.
//!
//! # Why not shadow-git
//!
//! The issue proposes shadow git repositories. That needs either a git library
//! (a supply-chain decision) or a host `git` binary (a new runtime dependency
//! that would itself need argv-exec confinement), and it buys history semantics
//! — branches, merges, blame — that an undo-the-last-write feature does not
//! use. A content-addressed blob store delivers the recoverable-prior-state
//! property with no new dependency at all. If history semantics are wanted
//! later, this store is what they would be built on rather than something that
//! has to be unpicked.
//!
//! # What this deliberately does not claim
//!
//! The snapshot id is returned in [`ToolOutput::receipt_data`], but the runtime
//! currently builds its `ToolCallReceipt` unconditionally and never reads that
//! field. So a snapshot is **not** yet linked into the receipt chain, and this
//! module does not pretend otherwise. Making that link real is a runtime
//! change, tracked separately.

use std::path::{Path, PathBuf};

use ardur_core_types::Sha256Digest;

/// Names the captured prior content of a file.
///
/// The id is the SHA-256 of the bytes, so identical prior content captured
/// twice occupies one blob and the id is reproducible from the content alone.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SnapshotId(String);

impl SnapshotId {
    /// The id for `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256Digest::of(bytes).to_hex())
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parse an id supplied by a caller.
    ///
    /// The field is private and this is the only way in, because the id is
    /// used to build a filesystem path. An empty or short value would panic
    /// the two-character shard split, and one containing path components
    /// would escape the store root — leaking the digest of an arbitrary
    /// readable file through the [`SnapshotError::Corrupt`] report.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::MalformedId`] unless `id` is exactly 64
    /// lowercase hex characters.
    pub fn parse(id: &str) -> Result<Self, SnapshotError> {
        let ok = id.len() == 64
            && id
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
        if ok {
            Ok(Self(id.to_string()))
        } else {
            Err(SnapshotError::MalformedId { id: id.to_string() })
        }
    }
}

/// What a capture attempt found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Snapshot {
    /// Prior content was captured under this id.
    Captured(SnapshotId),
    /// The path did not exist, so there was nothing to lose.
    ///
    /// Distinct from `Captured` of empty bytes: restoring "absent" means
    /// removing the file, restoring empty means truncating it. Collapsing the
    /// two would make a create indistinguishable from an overwrite-with-empty
    /// and would resurrect a file the undo should have removed.
    NothingToCapture,
}

/// Why a snapshot could not be taken or restored.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// The prior content could not be read.
    #[error("reading prior content of `{path}`: {source}")]
    Read {
        /// The file being captured.
        path: String,
        /// The underlying IO error.
        source: std::io::Error,
    },
    /// The snapshot store could not be written.
    #[error("writing snapshot `{id}`: {source}")]
    Write {
        /// The snapshot id.
        id: String,
        /// The underlying IO error.
        source: std::io::Error,
    },
    /// A caller-supplied id is not a well-formed digest.
    #[error(
        "`{id}` is not a snapshot id: expected 64 lowercase hex characters. \
         Ids are digests, and anything else would be used to build a path \
         inside the store"
    )]
    MalformedId {
        /// The rejected value.
        id: String,
    },
    /// The file is larger than the configured capture ceiling.
    #[error(
        "`{path}` is {size} bytes, over the {limit}-byte snapshot ceiling — \
         capturing it would read the whole file into memory, so the write was \
         refused rather than risking exhaustion"
    )]
    TooLarge {
        /// The file that was too big.
        path: String,
        /// Its size.
        size: u64,
        /// The configured ceiling.
        limit: u64,
    },
    /// The file on disk is not the one this snapshot was taken against.
    #[error(
        "refusing to restore: `{path}` is not the file this snapshot was taken \
         against (it has been changed since), so undoing would discard newer \
         work"
    )]
    Stale {
        /// The path that changed underneath.
        path: String,
    },
    /// The path cannot be snapshotted meaningfully.
    #[error("cannot snapshot `{path}`: {reason}")]
    Unsupported {
        /// The path.
        path: String,
        /// Why.
        reason: String,
    },
    /// The requested snapshot is not in the store.
    #[error("snapshot `{id}` is not in the store")]
    Missing {
        /// The snapshot id.
        id: String,
    },
    /// A stored blob's content does not match the id naming it.
    #[error(
        "snapshot `{id}` does not match its content digest `{actual}` — the \
         store has been modified, and restoring it would write bytes the id \
         does not name"
    )]
    Corrupt {
        /// The requested id.
        id: String,
        /// The digest the stored bytes actually hash to.
        actual: String,
    },
}

/// A content-addressed store of pre-write file content.
/// Default ceiling on a file this store will capture.
///
/// Capturing reads the whole file into memory, so an unbounded ceiling lets a
/// write of a few bytes to a multi-gigabyte artifact exhaust the process. 64
/// MiB keeps ordinary source and config files well inside the limit.
pub const DEFAULT_MAX_CAPTURE_BYTES: u64 = 64 * 1024 * 1024;

/// A content-addressed store of pre-write file content.
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    root: PathBuf,
    max_capture_bytes: u64,
}

impl SnapshotStore {
    /// A store rooted at `root`. The directory is created on first write.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            max_capture_bytes: DEFAULT_MAX_CAPTURE_BYTES,
        }
    }

    /// Set the largest file this store will capture.
    #[must_use]
    pub fn with_max_capture_bytes(mut self, limit: u64) -> Self {
        self.max_capture_bytes = limit;
        self
    }

    /// Where the store keeps its blobs.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create `dir` (and parents) owner-only.
    ///
    /// A snapshot holds the prior contents of a file that may have been mode
    /// 0600. Creating blobs under the ambient umask (commonly 0644) would
    /// publish those contents to every local account that can traverse the
    /// store.
    async fn create_dir_private(dir: &Path) -> std::io::Result<()> {
        tokio::fs::create_dir_all(dir).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await?;
        }
        Ok(())
    }

    fn blob_path(&self, id: &SnapshotId) -> PathBuf {
        // Two-character shard so a long-running session does not put hundreds
        // of thousands of entries in one directory.
        let (shard, rest) = id.0.split_at(2);
        self.root.join(shard).join(rest)
    }

    /// Remove blobs whose last modification is older than `max_age`.
    ///
    /// Review item 4: the store otherwise grows without bound, which is a disk
    /// leak for a long-running session. Pruning is a caller decision — there is
    /// no ambient policy here about how long an undo should stay available, and
    /// guessing one would silently discard recovery data an operator expected
    /// to keep.
    ///
    /// Returns the number of blobs removed.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError`] if the store cannot be traversed. A blob that
    /// vanishes mid-sweep is not an error: another pruner reaching it first is
    /// the intended outcome, not a failure.
    pub async fn prune_older_than(
        &self,
        max_age: std::time::Duration,
    ) -> Result<usize, SnapshotError> {
        let cutoff = match std::time::SystemTime::now().checked_sub(max_age) {
            Some(cutoff) => cutoff,
            // A max_age larger than the clock's epoch would prune everything;
            // refusing is safer than deleting the whole store on an arithmetic
            // edge case.
            None => return Ok(0),
        };

        let mut removed = 0usize;
        let mut shards = match tokio::fs::read_dir(&self.root).await {
            Ok(shards) => shards,
            // Nothing captured yet is nothing to prune.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(source) => {
                return Err(SnapshotError::Read {
                    path: self.root.display().to_string(),
                    source,
                });
            }
        };

        while let Ok(Some(shard)) = shards.next_entry().await {
            let mut blobs = match tokio::fs::read_dir(shard.path()).await {
                Ok(blobs) => blobs,
                Err(_) => continue,
            };
            while let Ok(Some(blob)) = blobs.next_entry().await {
                let Ok(meta) = blob.metadata().await else {
                    continue;
                };
                let Ok(modified) = meta.modified() else {
                    // A filesystem without mtime cannot be age-pruned; skipping
                    // keeps the blob rather than deleting on unknown age.
                    continue;
                };
                if modified < cutoff && tokio::fs::remove_file(blob.path()).await.is_ok() {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// Write `bytes` to `path` owner-only.
    async fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        tokio::fs::write(path, bytes).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
        }
        Ok(())
    }

    /// Capture the current content of `path`, if it has any.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError`] if an existing file cannot be read or the
    /// store cannot be written. A missing file is [`Snapshot::NothingToCapture`],
    /// not an error.
    pub async fn capture(&self, path: &Path) -> Result<Snapshot, SnapshotError> {
        // Refuse before allocating: a small write to a huge file must not be
        // able to exhaust the process (review P1 on capture memory). Use
        // symlink_metadata so a dangling link is classified below rather than
        // reported as a missing file.
        match tokio::fs::symlink_metadata(path).await {
            Ok(meta) if meta.is_file() && meta.len() > self.max_capture_bytes => {
                return Err(SnapshotError::TooLarge {
                    path: path.display().to_string(),
                    size: meta.len(),
                    limit: self.max_capture_bytes,
                });
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                // A symlink is a directory entry in its own right. Writing
                // through it creates or edits the TARGET, so capturing the
                // link as "absent" and later deleting it would remove a
                // pre-existing entry while leaving the written target behind.
                return Err(SnapshotError::Unsupported {
                    path: path.display().to_string(),
                    reason: "the path is a symlink; writing through it changes the target, \
                             so an undo cannot be expressed as restoring this entry"
                        .to_string(),
                });
            }
            _ => {}
        }

        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Snapshot::NothingToCapture);
            }
            Err(source) => {
                // A read failure is NOT treated as "nothing to capture": that
                // would silently proceed with a write whose prior state was
                // unrecoverable, which is the case the snapshot exists for.
                return Err(SnapshotError::Read {
                    path: path.display().to_string(),
                    source,
                });
            }
        };

        let id = SnapshotId::of(&bytes);
        let blob = self.blob_path(&id);

        // An existing blob is only reusable if its bytes still hash to the id.
        // A truncated or tampered blob would otherwise be accepted here, the
        // source file destroyed by the write, and the corruption discovered
        // only at restore — exactly when the original is unrecoverable.
        if let Ok(existing) = tokio::fs::read(&blob).await {
            if SnapshotId::of(&existing) == id {
                return Ok(Snapshot::Captured(id));
            }
            // Fall through and rewrite it from the bytes we just read.
        }

        if let Some(parent) = blob.parent() {
            Self::create_dir_private(parent)
                .await
                .map_err(|source| SnapshotError::Write {
                    id: id.0.clone(),
                    source,
                })?;
        }

        // Write to a temporary name and rename into place, so a cancelled or
        // crashed capture never leaves a half-written blob that a later
        // capture would accept.
        let tmp = blob.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
        Self::write_private(&tmp, &bytes)
            .await
            .map_err(|source| SnapshotError::Write {
                id: id.0.clone(),
                source,
            })?;
        tokio::fs::rename(&tmp, &blob)
            .await
            .map_err(|source| SnapshotError::Write {
                id: id.0.clone(),
                source,
            })?;

        Ok(Snapshot::Captured(id))
    }

    /// The bytes a snapshot holds.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::Missing`] if the id is unknown,
    /// [`SnapshotError::Corrupt`] if the stored bytes do not hash to the id,
    /// or [`SnapshotError::Read`] on an IO failure.
    pub async fn read(&self, id: &SnapshotId) -> Result<Vec<u8>, SnapshotError> {
        let blob = self.blob_path(id);
        let bytes = match tokio::fs::read(&blob).await {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(SnapshotError::Missing { id: id.0.clone() });
            }
            Err(source) => {
                return Err(SnapshotError::Read {
                    path: blob.display().to_string(),
                    source,
                });
            }
        };

        // The store is a plain directory, so a blob can be altered by anything
        // with write access. Re-deriving the digest before handing the bytes
        // back means a tampered store is refused rather than restored: an undo
        // that writes attacker-chosen content is worse than no undo.
        let actual = SnapshotId::of(&bytes);
        if actual != *id {
            return Err(SnapshotError::Corrupt {
                id: id.0.clone(),
                actual: actual.0,
            });
        }
        Ok(bytes)
    }

    /// Restore `snapshot` over `path`.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError`] if the snapshot is missing or corrupt, or the
    /// write fails.
    pub async fn restore(&self, snapshot: &Snapshot, path: &Path) -> Result<(), SnapshotError> {
        match snapshot {
            Snapshot::Captured(id) => {
                let bytes = self.read(id).await?;
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await.map_err(|source| {
                        SnapshotError::Write {
                            id: id.0.clone(),
                            source,
                        }
                    })?;
                }
                tokio::fs::write(path, &bytes)
                    .await
                    .map_err(|source| SnapshotError::Write {
                        id: id.0.clone(),
                        source,
                    })
            }
            // The file did not exist before the write, so undoing the write
            // means removing it — not leaving an empty file behind.
            //
            // But only if it is still the file the write created. An undo that
            // arrives after someone else has edited or replaced the path would
            // otherwise silently delete newer work. `restore_expecting` is the
            // checked form; this unconditional branch is reachable only when a
            // caller explicitly opts out of the check.
            Snapshot::NothingToCapture => match tokio::fs::remove_file(path).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(source) => Err(SnapshotError::Write {
                    id: "absent".to_string(),
                    source,
                }),
            },
        }
    }
}

impl SnapshotStore {
    /// Restore `snapshot` over `path`, but only if the file there is still the
    /// one the write produced.
    ///
    /// `written` is the digest of the content the write left behind. If the
    /// file no longer matches it, someone has changed the path since and the
    /// restore is refused with [`SnapshotError::Stale`] rather than discarding
    /// their work. A file that has since been deleted is treated as already
    /// undone.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError`] if the snapshot is missing or corrupt, the
    /// current file does not match `written`, or the write fails.
    pub async fn restore_expecting(
        &self,
        snapshot: &Snapshot,
        path: &Path,
        written: &SnapshotId,
    ) -> Result<(), SnapshotError> {
        match tokio::fs::read(path).await {
            Ok(current) => {
                if SnapshotId::of(&current) != *written {
                    return Err(SnapshotError::Stale {
                        path: path.display().to_string(),
                    });
                }
            }
            // Already gone: nothing of the write survives to undo.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return match snapshot {
                    Snapshot::NothingToCapture => Ok(()),
                    Snapshot::Captured(_) => self.restore(snapshot, path).await,
                };
            }
            Err(source) => {
                return Err(SnapshotError::Read {
                    path: path.display().to_string(),
                    source,
                });
            }
        }
        self.restore(snapshot, path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> (tempfile::TempDir, SnapshotStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        (dir, store)
    }

    #[tokio::test]
    async fn prior_content_survives_an_overwrite() {
        let (dir, store) = store().await;
        let file = dir.path().join("note.md");
        tokio::fs::write(&file, b"original").await.expect("seed");

        let snap = store.capture(&file).await.expect("capture succeeds");
        tokio::fs::write(&file, b"clobbered").await.expect("write");

        store.restore(&snap, &file).await.expect("restore succeeds");
        assert_eq!(
            tokio::fs::read(&file).await.expect("read back"),
            b"original",
            "the prior content must come back byte for byte"
        );
    }

    #[tokio::test]
    async fn a_missing_file_captures_as_absent_and_restores_by_removal() {
        // The distinction that matters: restoring "absent" must REMOVE the
        // file. Treating a missing file as empty content would resurrect a file
        // the undo should have deleted.
        let (dir, store) = store().await;
        let file = dir.path().join("new.md");

        let snap = store.capture(&file).await.expect("capture succeeds");
        assert_eq!(snap, Snapshot::NothingToCapture);

        tokio::fs::write(&file, b"created").await.expect("write");
        store.restore(&snap, &file).await.expect("restore succeeds");

        assert!(
            !file.exists(),
            "undoing the creation of a file must remove it, not leave it empty"
        );
    }

    #[tokio::test]
    async fn an_empty_file_is_not_the_same_as_an_absent_one() {
        let (dir, store) = store().await;
        let file = dir.path().join("empty.md");
        tokio::fs::write(&file, b"").await.expect("seed empty");

        let snap = store.capture(&file).await.expect("capture succeeds");
        assert!(
            matches!(snap, Snapshot::Captured(_)),
            "an existing empty file has content to restore: {snap:?}"
        );

        tokio::fs::write(&file, b"filled").await.expect("write");
        store.restore(&snap, &file).await.expect("restore succeeds");

        assert!(
            file.exists(),
            "restoring an empty file must keep it present"
        );
        assert_eq!(tokio::fs::read(&file).await.expect("read back"), b"");
    }

    #[tokio::test]
    async fn identical_content_shares_one_blob() {
        let (dir, store) = store().await;
        let a = dir.path().join("a.md");
        let b = dir.path().join("b.md");
        tokio::fs::write(&a, b"same").await.expect("seed a");
        tokio::fs::write(&b, b"same").await.expect("seed b");

        let snap_a = store.capture(&a).await.expect("capture a");
        let snap_b = store.capture(&b).await.expect("capture b");
        assert_eq!(snap_a, snap_b, "content addressing means one id");
    }

    #[tokio::test]
    async fn a_tampered_blob_is_refused_rather_than_restored() {
        // An undo that writes attacker-chosen content is worse than no undo.
        let (dir, store) = store().await;
        let file = dir.path().join("note.md");
        tokio::fs::write(&file, b"original").await.expect("seed");

        let snap = store.capture(&file).await.expect("capture succeeds");
        let Snapshot::Captured(id) = &snap else {
            panic!("expected a capture");
        };

        // Overwrite the stored blob with different bytes.
        let blob = store.blob_path(id);
        tokio::fs::write(&blob, b"malicious").await.expect("tamper");

        let err = store
            .restore(&snap, &file)
            .await
            .expect_err("a tampered blob must be refused");
        assert!(
            matches!(err, SnapshotError::Corrupt { .. }),
            "expected Corrupt, got {err:?}"
        );
        assert_eq!(
            tokio::fs::read(&file).await.expect("read back"),
            b"original",
            "and the target must be left untouched"
        );
    }

    #[tokio::test]
    async fn an_unknown_snapshot_is_missing_not_silently_ignored() {
        let (dir, store) = store().await;
        let file = dir.path().join("note.md");
        let ghost = Snapshot::Captured(SnapshotId::of(b"never captured"));

        let err = store
            .restore(&ghost, &file)
            .await
            .expect_err("an unknown id must be refused");
        assert!(matches!(err, SnapshotError::Missing { .. }));
    }

    #[tokio::test]
    async fn an_unreadable_file_is_an_error_not_a_silent_skip() {
        // Treating a read failure as "nothing to capture" would let the write
        // proceed with its prior state unrecoverable — exactly the case the
        // snapshot exists to prevent.
        let (dir, store) = store().await;
        let path = dir.path().join("a-directory");
        tokio::fs::create_dir(&path).await.expect("mkdir");

        // Reading a directory as a file fails with something other than
        // NotFound.
        let result = store.capture(&path).await;
        assert!(
            matches!(result, Err(SnapshotError::Read { .. })),
            "expected a Read error, got {result:?}"
        );
    }

    #[tokio::test]
    async fn pruning_removes_old_blobs_and_keeps_recent_ones() {
        let (dir, store) = store().await;
        let file = dir.path().join("note.md");
        tokio::fs::write(&file, b"kept").await.expect("seed");
        let snap = store.capture(&file).await.expect("capture");

        // Nothing is old yet, so a long max_age prunes nothing.
        assert_eq!(
            store
                .prune_older_than(std::time::Duration::from_secs(3600))
                .await
                .expect("prune succeeds"),
            0,
            "a fresh blob must not be pruned"
        );
        assert!(
            store.restore(&snap, &file).await.is_ok(),
            "and it must still be restorable"
        );

        // A zero max_age makes everything older than the cutoff.
        let removed = store
            .prune_older_than(std::time::Duration::ZERO)
            .await
            .expect("prune succeeds");
        assert!(removed >= 1, "an aged-out blob must be removed");

        let err = store
            .restore(&snap, &file)
            .await
            .expect_err("a pruned snapshot is gone");
        assert!(
            matches!(err, SnapshotError::Missing { .. }),
            "a pruned blob must report Missing rather than silently succeeding: {err:?}"
        );
    }

    #[tokio::test]
    async fn pruning_an_empty_store_is_not_an_error() {
        let (_dir, store) = store().await;
        assert_eq!(
            store
                .prune_older_than(std::time::Duration::ZERO)
                .await
                .expect("pruning nothing succeeds"),
            0
        );
    }
}
