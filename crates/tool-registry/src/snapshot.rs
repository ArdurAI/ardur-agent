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
pub struct SnapshotId(pub String);

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
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    root: PathBuf,
}

impl SnapshotStore {
    /// A store rooted at `root`. The directory is created on first write.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Where the store keeps its blobs.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn blob_path(&self, id: &SnapshotId) -> PathBuf {
        // Two-character shard so a long-running session does not put hundreds
        // of thousands of entries in one directory.
        let (shard, rest) = id.0.split_at(2);
        self.root.join(shard).join(rest)
    }

    /// Capture the current content of `path`, if it has any.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError`] if an existing file cannot be read or the
    /// store cannot be written. A missing file is [`Snapshot::NothingToCapture`],
    /// not an error.
    pub async fn capture(&self, path: &Path) -> Result<Snapshot, SnapshotError> {
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

        // Content-addressed: if the blob is already there its bytes are the
        // same bytes, so rewriting it is pure cost.
        if tokio::fs::metadata(&blob).await.is_ok() {
            return Ok(Snapshot::Captured(id));
        }

        if let Some(parent) = blob.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| SnapshotError::Write {
                    id: id.0.clone(),
                    source,
                })?;
        }
        tokio::fs::write(&blob, &bytes)
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
}
