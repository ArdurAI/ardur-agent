//! On-disk schema versioning for a state tree (`StateDirs`' `~/.ardur`, or an
//! equivalent server `data_dir`).
//!
//! A single `SCHEMA_VERSION` file at the tree's root holds the on-disk shape
//! version as a bare decimal integer. [`migrate_to`] reads it, walks a
//! caller-supplied chain of [`Migration`] steps from the version found to a
//! target version, and writes the new version back atomically — through
//! [`crate::write_atomic_no_follow`], so a crash mid-migration never leaves a
//! half-migrated tree stamped as if it had finished.
//!
//! No migration ships in this crate: `ardur-durability` only owns the
//! version marker and the walk. Each consumer (the CLI's `StateDirs`, the
//! server's `data_dir` boot, `ardur backup restore`) supplies its own target
//! version and migration list, so a future on-disk format change is a new
//! [`Migration`] entry at the call site, not a change to this crate.

use std::path::Path;

use crate::DurabilityError;

/// The version assumed for a state tree that predates this file's
/// existence: every `SCHEMA_VERSION`-aware consumer stamps the file on every
/// boot from this crate's introduction onward, so an unstamped tree found
/// later can only mean "created before schema versioning shipped" — the
/// baseline shape, fixed for all time regardless of how many versions ship
/// after it.
pub const UNVERSIONED_BASELINE: u32 = 1;

/// The file name, relative to a state tree's root, holding the version.
pub const VERSION_FILE_NAME: &str = "SCHEMA_VERSION";

/// One on-disk format transition. `apply` receives the state tree's root and
/// must leave it in the `to` shape; it runs before the version file is
/// updated, so a failed migration leaves the tree stamped at `from` (safe to
/// retry) rather than falsely stamped `to`.
pub struct Migration {
    /// The version this migration applies from.
    pub from: u32,
    /// The version this migration produces.
    pub to: u32,
    /// Human-readable description, surfaced in [`MigrationReport::applied`].
    pub description: &'static str,
    /// Transform the state tree at `root` from `from`'s shape to `to`'s.
    pub apply: fn(root: &Path) -> Result<(), DurabilityError>,
}

/// What [`migrate_to`] did.
#[derive(Debug, Clone)]
pub struct MigrationReport {
    /// The version found on disk before migrating (or [`UNVERSIONED_BASELINE`]
    /// if no version file existed).
    pub from: u32,
    /// The version now stamped on disk.
    pub to: u32,
    /// Descriptions of every migration step applied, in order. Empty if the
    /// tree was already at the target version (including a freshly stamped
    /// unversioned tree that happened to equal the target).
    pub applied: Vec<&'static str>,
}

/// Read the version stamped at `root/SCHEMA_VERSION`. `Ok(None)` means no
/// version file exists yet — a fresh state tree, or one created before
/// schema versioning shipped.
pub fn read_version(root: &Path) -> Result<Option<u32>, DurabilityError> {
    let path = root.join(VERSION_FILE_NAME);
    match crate::read_string_no_follow(&path) {
        Ok(contents) => contents.trim().parse::<u32>().map(Some).map_err(|e| {
            DurabilityError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} does not contain a valid version: {e}", path.display()),
            ))
        }),
        Err(e) if e.is_not_found() => Ok(None),
        Err(e) => Err(e),
    }
}

/// Atomically stamp `root/SCHEMA_VERSION` with `version`.
pub fn write_version(root: &Path, version: u32) -> Result<(), DurabilityError> {
    let path = root.join(VERSION_FILE_NAME);
    crate::write_atomic_no_follow(&path, version.to_string().as_bytes())
}

/// Migrate the state tree at `root` to `target`, applying `migrations` in
/// order starting from whatever version is currently stamped (or
/// [`UNVERSIONED_BASELINE`] if unstamped). Idempotent: a tree already at
/// `target` is left alone (aside from stamping an unversioned one). Fails
/// closed with [`DurabilityError::MigrationGap`] if no migration covers the
/// version reached — the tree is left at whatever version it last
/// successfully reached, never silently skipped forward.
pub fn migrate_to(
    root: &Path,
    target: u32,
    migrations: &[Migration],
) -> Result<MigrationReport, DurabilityError> {
    let from = read_version(root)?.unwrap_or(UNVERSIONED_BASELINE);
    let mut current = from;
    let mut applied = Vec::new();
    while current != target {
        let step =
            migrations
                .iter()
                .find(|m| m.from == current)
                .ok_or(DurabilityError::MigrationGap {
                    at: current,
                    target,
                })?;
        (step.apply)(root)?;
        current = step.to;
        applied.push(step.description);
    }
    write_version(root, current)?;
    Ok(MigrationReport {
        from,
        to: current,
        applied,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().canonicalize().expect("canonical tempdir root")
    }

    #[test]
    fn unversioned_tree_is_stamped_at_target_with_no_migrations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = root(&dir);
        std::fs::create_dir_all(&root).unwrap();

        let report = migrate_to(&root, UNVERSIONED_BASELINE, &[]).expect("migrate");
        assert_eq!(report.from, UNVERSIONED_BASELINE);
        assert_eq!(report.to, UNVERSIONED_BASELINE);
        assert!(report.applied.is_empty());
        assert_eq!(read_version(&root).unwrap(), Some(UNVERSIONED_BASELINE));
    }

    #[test]
    fn already_at_target_is_a_no_op_beyond_stamping() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = root(&dir);
        std::fs::create_dir_all(&root).unwrap();
        write_version(&root, 3).unwrap();

        let report = migrate_to(&root, 3, &[]).expect("already at target");
        assert_eq!(report.from, 3);
        assert_eq!(report.to, 3);
        assert!(report.applied.is_empty());
    }

    #[test]
    fn walks_a_multi_step_migration_chain_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = root(&dir);
        std::fs::create_dir_all(&root).unwrap();
        write_version(&root, 1).unwrap();

        fn mark(root: &Path, name: &str) -> Result<(), DurabilityError> {
            crate::write_atomic_no_follow(&root.join(name), b"done")
        }
        fn step_1_to_2(root: &Path) -> Result<(), DurabilityError> {
            mark(root, "migrated-1-to-2")
        }
        fn step_2_to_3(root: &Path) -> Result<(), DurabilityError> {
            mark(root, "migrated-2-to-3")
        }

        let migrations = [
            Migration {
                from: 1,
                to: 2,
                description: "1-to-2",
                apply: step_1_to_2,
            },
            Migration {
                from: 2,
                to: 3,
                description: "2-to-3",
                apply: step_2_to_3,
            },
        ];

        let report = migrate_to(&root, 3, &migrations).expect("chain applies");
        assert_eq!(report.from, 1);
        assert_eq!(report.to, 3);
        assert_eq!(report.applied, vec!["1-to-2", "2-to-3"]);
        assert!(root.join("migrated-1-to-2").is_file());
        assert!(root.join("migrated-2-to-3").is_file());
        assert_eq!(read_version(&root).unwrap(), Some(3));
    }

    #[test]
    fn a_gap_in_the_migration_chain_fails_closed_without_advancing_the_stamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = root(&dir);
        std::fs::create_dir_all(&root).unwrap();
        write_version(&root, 1).unwrap();

        // No migration registered for version 1 at all.
        let err = migrate_to(&root, 2, &[]).expect_err("must fail closed on a gap");
        assert!(matches!(
            err,
            DurabilityError::MigrationGap { at: 1, target: 2 }
        ));
        // The stamp must not have advanced past the last successful version.
        assert_eq!(read_version(&root).unwrap(), Some(1));
    }

    #[test]
    fn a_failing_migration_step_leaves_the_stamp_at_the_last_good_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = root(&dir);
        std::fs::create_dir_all(&root).unwrap();
        write_version(&root, 1).unwrap();

        fn always_fails(_root: &Path) -> Result<(), DurabilityError> {
            Err(DurabilityError::Io(std::io::Error::other("boom")))
        }
        let migrations = [Migration {
            from: 1,
            to: 2,
            description: "flaky",
            apply: always_fails,
        }];

        assert!(migrate_to(&root, 2, &migrations).is_err());
        assert_eq!(
            read_version(&root).unwrap(),
            Some(1),
            "a failed migration must not falsely advance the stamp"
        );
    }
}
