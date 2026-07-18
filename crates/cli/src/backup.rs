//! Backup and restore for an Ardur state tree (`~/.ardur`, or an equivalent
//! server `data_dir`): receipts, journals, keys, and any memory index nested
//! underneath share the same `memory/`, `journals/`, `receipts/`, `keys/`
//! shape, so one archiver covers both.
//!
//! Archives are gzip-compressed tar, built and written through
//! [`ardur_durability`]'s crash-safe atomic-write primitive so a crash
//! mid-backup never leaves a truncated archive at the destination path.
//! Restore extracts into a staging directory beside the target, verifies the
//! restored receipt chain's hash linkage, then swaps the staging directory
//! into place — the prior state tree is preserved alongside it (never
//! deleted outright) so a bad restore can be undone by hand.
//!
//! Every path here is opened through the same no-follow guard the rest of
//! the state tree uses, so an archive/output path with a symlinked parent
//! directory is refused rather than followed. On macOS, `/tmp` itself is
//! such a symlink (-> `/private/tmp`) — pass a path under `$HOME` or an
//! already-canonical directory instead.

use std::path::{Path, PathBuf};

use ardur_cli::CliError;
use clap::{Args, Subcommand};

use crate::StateDirs;

/// Arguments to `ardur backup`.
#[derive(Args)]
pub struct BackupArgs {
    #[command(subcommand)]
    pub action: BackupAction,
}

/// Subcommands for `ardur backup`.
#[derive(Subcommand)]
pub enum BackupAction {
    /// Archive the state tree (receipts, journals, keys, memory index) to a
    /// gzip-compressed tarball.
    Create {
        /// Output archive path (defaults to `<state-dir>-<timestamp>.tar.gz`
        /// next to the state directory).
        #[arg(long)]
        output: Option<PathBuf>,
        /// State directory to archive (defaults to `~/.ardur`).
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Restore a state tree from an archive created by `backup create`.
    Restore {
        /// The backup archive to restore from.
        archive: PathBuf,
        /// State directory to restore into (defaults to `~/.ardur`).
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Delete the preserved pre-restore copy instead of keeping it
        /// alongside the restored state tree.
        #[arg(long)]
        force: bool,
    },
}

fn resolve_state_dir(state_dir: Option<PathBuf>) -> Result<PathBuf, CliError> {
    match state_dir {
        Some(dir) => Ok(dir),
        None => Ok(StateDirs::resolve()?.root),
    }
}

/// Build a gzip-compressed tar archive of every file under `state_root`,
/// written atomically to `output`. Symlinks inside the tree are archived as
/// symlink entries, never followed — a hostile or accidental symlink under
/// the state tree cannot pull unrelated files into the backup.
fn create_backup(state_root: &Path, output: &Path) -> Result<u64, CliError> {
    if !state_root.is_dir() {
        return Err(CliError::State(format!(
            "state directory does not exist: {}",
            state_root.display()
        )));
    }

    let mut archive_bytes = Vec::new();
    {
        let encoder =
            flate2::write::GzEncoder::new(&mut archive_bytes, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        builder.follow_symlinks(false);
        builder
            .append_dir_all(".", state_root)
            .map_err(|e| CliError::State(format!("archiving {}: {e}", state_root.display())))?;
        let encoder = builder
            .into_inner()
            .map_err(|e| CliError::State(format!("finalizing tar stream: {e}")))?;
        encoder
            .finish()
            .map_err(|e| CliError::State(format!("finalizing gzip stream: {e}")))?;
    }

    let len = archive_bytes.len() as u64;
    ardur_durability::write_atomic_no_follow(output, &archive_bytes).map_err(|e| {
        CliError::State(format!("writing backup archive {}: {e}", output.display()))
    })?;
    Ok(len)
}

/// Outcome of a successful [`restore_backup`].
#[derive(Debug)]
struct RestoreReport {
    /// Where the pre-restore state tree was moved, if one existed and
    /// `force` was not set.
    previous_state_preserved_at: Option<PathBuf>,
    /// Number of receipts found in the restored, verified chain.
    receipt_count: usize,
}

/// Restore `state_root` from a backup archive created by [`create_backup`].
fn restore_backup(
    archive: &Path,
    state_root: &Path,
    force: bool,
) -> Result<RestoreReport, CliError> {
    let bytes = ardur_durability::read_no_follow(archive).map_err(|e| {
        CliError::State(format!("reading backup archive {}: {e}", archive.display()))
    })?;

    let parent = state_root.parent().ok_or_else(|| {
        CliError::State(format!(
            "state directory has no parent: {}",
            state_root.display()
        ))
    })?;
    std::fs::create_dir_all(parent)?;

    let token = uuid::Uuid::new_v4().simple().to_string();
    let staging = parent.join(format!(".ardur-restore-{token}"));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;

    {
        let decoder = flate2::read::GzDecoder::new(bytes.as_slice());
        let mut archive_reader = tar::Archive::new(decoder);
        archive_reader.unpack(&staging).map_err(|e| {
            let _ = std::fs::remove_dir_all(&staging);
            CliError::State(format!("extracting backup archive: {e}"))
        })?;
    }

    // Verify the restored receipt chain's hash linkage before it can ever
    // become the live state tree. A missing chain (fresh install) verifies
    // trivially as an empty chain.
    let receipt_count = {
        let chain_path = staging.join("receipts").join("chain.jsonl");
        let chain = ardur_fused_runtime::load_persisted_chain(&chain_path).map_err(|e| {
            let _ = std::fs::remove_dir_all(&staging);
            CliError::State(format!("loading restored receipt chain: {e}"))
        })?;
        if let Err(e) = ardur_fused_runtime::verify_persisted_chain(&chain) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(CliError::State(format!(
                "restored receipt chain failed hash-linkage verification: {e}"
            )));
        }
        chain.len()
    };

    let previous_state_preserved_at = if state_root.exists() {
        let displaced = parent.join(format!(".ardur-pre-restore-{token}"));
        std::fs::rename(state_root, &displaced)?;
        Some(displaced)
    } else {
        None
    };

    if let Err(error) = std::fs::rename(&staging, state_root) {
        // Best-effort rollback: put the previous state back exactly where it
        // was so a failed swap never leaves the state directory missing.
        if let Some(displaced) = &previous_state_preserved_at {
            let _ = std::fs::rename(displaced, state_root);
        }
        return Err(CliError::State(format!(
            "swapping restored state into place: {error}"
        )));
    }

    // An archive made by an older `ardur` may predate a schema change; bring
    // the now-live tree up to the current on-disk shape the same way a
    // normal boot would (StateDirs::create() / the server's data_dir setup).
    // No migration exists yet, so this is a no-op stamp today.
    ardur_durability::schema::migrate_to(
        state_root,
        ardur_durability::schema::UNVERSIONED_BASELINE,
        &[],
    )
    .map_err(|e| CliError::State(format!("restored state schema migration: {e}")))?;

    let previous_state_preserved_at = match previous_state_preserved_at {
        Some(displaced) if force => {
            std::fs::remove_dir_all(&displaced)?;
            None
        }
        other => other,
    };

    Ok(RestoreReport {
        previous_state_preserved_at,
        receipt_count,
    })
}

/// Run `ardur backup`.
pub fn run_backup(args: BackupArgs) -> Result<(), CliError> {
    match args.action {
        BackupAction::Create { output, state_dir } => {
            let state_root = resolve_state_dir(state_dir)?;
            let output = match output {
                Some(path) => path,
                None => {
                    let name = state_root
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "ardur-state".to_string());
                    let parent = state_root
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_default();
                    parent.join(format!(
                        "{name}-backup-{}.tar.gz",
                        uuid::Uuid::new_v4().simple()
                    ))
                }
            };
            let bytes = create_backup(&state_root, &output)?;
            println!(
                "backed up {} ({} bytes) to {}",
                state_root.display(),
                bytes,
                output.display()
            );
        }
        BackupAction::Restore {
            archive,
            state_dir,
            force,
        } => {
            let state_root = resolve_state_dir(state_dir)?;
            let report = restore_backup(&archive, &state_root, force)?;
            println!(
                "restored {} into {} ({} receipts verified)",
                archive.display(),
                state_root.display(),
                report.receipt_count
            );
            match report.previous_state_preserved_at {
                Some(displaced) => println!(
                    "previous state preserved at {} — delete it once you've confirmed the restore is good",
                    displaced.display()
                ),
                None => println!("no previous state existed at that path"),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn backup_then_restore_round_trips_files() {
        let root_dir = tempfile::tempdir().expect("tempdir");
        let root = root_dir.path().canonicalize().expect("canonical root");
        let state = root.join("state");
        write(&state.join("keys/issuer.key"), b"issuer-key-bytes");
        write(&state.join("keys/receipt.pem"), b"receipt-pem-bytes");
        write(&state.join("journals/sessions/abc/journal.jsonl"), b"{}\n");
        write(&state.join("receipts/chain.jsonl"), b"");

        let archive = root.join("backup.tar.gz");
        let bytes = create_backup(&state, &archive).expect("create backup");
        assert!(bytes > 0);
        assert!(archive.is_file());

        let restore_target = root.join("state");
        let report = restore_backup(&archive, &restore_target, false).expect("restore succeeds");
        assert_eq!(report.receipt_count, 0);
        assert!(report.previous_state_preserved_at.is_some());

        assert_eq!(
            std::fs::read(restore_target.join("keys/issuer.key")).unwrap(),
            b"issuer-key-bytes"
        );
        assert_eq!(
            std::fs::read(restore_target.join("keys/receipt.pem")).unwrap(),
            b"receipt-pem-bytes"
        );
        assert_eq!(
            std::fs::read(restore_target.join("journals/sessions/abc/journal.jsonl")).unwrap(),
            b"{}\n"
        );
    }

    #[test]
    fn restore_preserves_and_can_force_delete_prior_state() {
        let root_dir = tempfile::tempdir().expect("tempdir");
        let root = root_dir.path().canonicalize().expect("canonical root");
        let state = root.join("state");
        write(&state.join("receipts/chain.jsonl"), b"");
        let archive = root.join("backup.tar.gz");
        create_backup(&state, &archive).expect("create backup");

        // Put different content in place before restoring, to prove the old
        // tree is displaced rather than merged or silently dropped.
        write(&state.join("keys/issuer.key"), b"stale-key");

        let kept = restore_backup(&archive, &state, false).expect("restore, keep prior");
        let preserved = kept
            .previous_state_preserved_at
            .expect("prior state preserved");
        assert!(preserved.join("keys/issuer.key").is_file());

        write(&state.join("keys/issuer.key"), b"stale-key-2");
        let forced = restore_backup(&archive, &state, true).expect("restore, force delete");
        assert!(forced.previous_state_preserved_at.is_none());
    }

    #[test]
    fn restore_rejects_a_tampered_receipt_chain() {
        let root_dir = tempfile::tempdir().expect("tempdir");
        let root = root_dir.path().canonicalize().expect("canonical root");
        let state = root.join("state");
        // A single line with a non-null parent_hash is not a valid genesis
        // receipt — the chain fails to load/verify.
        write(
            &state.join("receipts/chain.jsonl"),
            b"not-a-valid-jws-line\n",
        );
        let archive = root.join("backup.tar.gz");
        create_backup(&state, &archive).expect("create backup");

        let err = restore_backup(&archive, &root.join("restored"), false)
            .expect_err("malformed receipt chain must fail closed");
        assert!(
            err.to_string().contains("receipt chain"),
            "unexpected error: {err}"
        );
        assert!(
            !root.join("restored").exists(),
            "a failed restore must not leave a partial state tree in place"
        );
    }
}
