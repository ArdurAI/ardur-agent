//! Crash-safe, symlink-resistant file I/O primitives for durable agent state.
//!
//! Every helper here opens each path component descriptor-relatively with
//! `O_NOFOLLOW` (Unix) so neither a substituted parent directory nor a
//! substituted final component can redirect a write outside the intended
//! state tree. Every write that must survive a crash goes through a
//! temp-file-in-same-directory-then-rename sequence, fsyncing the temp file
//! and the parent directory so the rename itself is durable, not just atomic.
//!
//! This crate exists because three call sites (`crates/cli/src/secure_io.rs`,
//! `crates/fused-runtime/src/receipts.rs`, `crates/server/src/state.rs`)
//! independently reimplemented overlapping fragments of this pattern with
//! drifting completeness — most notably, the server's key-file writer used
//! a bare `O_EXCL` create with no tempfile/rename step, so a crash between
//! `openat` and `sync_all` left a truncated `issuer.key`/`receipt.pem` with
//! no recovery path on the next boot.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Component, Path};

#[cfg(unix)]
use std::ffi::{OsStr, OsString};

pub mod schema;

/// A failure from a durability primitive, distinct from a plain [`io::Error`]
/// so callers can tell a symlink refusal apart from an ordinary I/O failure.
#[derive(Debug, thiserror::Error)]
pub enum DurabilityError {
    /// A path component resolved through a symlink where one was refused.
    #[error("refusing symlink in trusted state path: {0}")]
    SymlinkRefused(String),
    /// The target existed and was not a regular file.
    #[error("refusing non-regular file: {0}")]
    NotRegularFile(String),
    /// [`schema::migrate_to`] found no migration covering the version
    /// reached, short of the requested target.
    #[error("no migration from schema version {at} toward target {target}")]
    MigrationGap {
        /// The version last successfully reached (still stamped on disk).
        at: u32,
        /// The version [`schema::migrate_to`] was asked to reach.
        target: u32,
    },
    /// Any other I/O failure.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl DurabilityError {
    /// True if this failure indicates the target path did not exist.
    pub fn is_not_found(&self) -> bool {
        matches!(self, DurabilityError::Io(e) if e.kind() == io::ErrorKind::NotFound)
    }
}

fn ensure_regular_file(file: &File, path: &Path) -> Result<(), DurabilityError> {
    if !file.metadata()?.file_type().is_file() {
        return Err(DurabilityError::NotRegularFile(path.display().to_string()));
    }
    Ok(())
}

/// True if `error` is how this platform reports "refused to follow a
/// symlink" from an `O_NOFOLLOW` open. Linux uses `ELOOP` for a symlinked
/// final component; combined with `O_DIRECTORY`, macOS/BSD instead report
/// `ENOTDIR` (the symlink, not followed, is "not a directory"). Both mean
/// the same thing here: a component that should have been a plain directory
/// or regular file was a symlink.
#[cfg(unix)]
fn is_symlink_refusal(error: rustix::io::Errno) -> bool {
    error == rustix::io::Errno::LOOP || error == rustix::io::Errno::NOTDIR
}

#[cfg(unix)]
fn errno_to_error(error: rustix::io::Errno, path: &Path) -> DurabilityError {
    if is_symlink_refusal(error) {
        DurabilityError::SymlinkRefused(path.display().to_string())
    } else {
        DurabilityError::Io(io::Error::from_raw_os_error(error.raw_os_error()))
    }
}

#[cfg(unix)]
fn open_directory_at(parent: &File, name: &OsStr, create: bool) -> Result<File, rustix::io::Errno> {
    use rustix::fs::{Mode, OFlags, mkdirat, openat};

    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match openat(parent, name, flags, Mode::empty()) {
        Ok(fd) => Ok(fd.into()),
        Err(error) if create && error == rustix::io::Errno::NOENT => {
            match mkdirat(parent, name, Mode::from_bits_truncate(0o700)) {
                Ok(()) => {}
                Err(error) if error == rustix::io::Errno::EXIST => {}
                Err(error) => return Err(error),
            }
            openat(parent, name, flags, Mode::empty()).map(File::from)
        }
        Err(error) => Err(error),
    }
}

/// Open the parent directory of `path`, walking every component
/// descriptor-relatively so no component may be a symlink. Returns the open
/// parent descriptor and the final path component's file name.
#[cfg(unix)]
fn open_parent_directory(path: &Path, create: bool) -> Result<(File, OsString), DurabilityError> {
    let file_name = path.file_name().ok_or_else(|| {
        DurabilityError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path has no file name: {}", path.display()),
        ))
    })?;
    let mut current = if path.is_absolute() {
        File::open("/")?
    } else {
        File::open(".")?
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    for component in parent.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                current = open_directory_at(&current, name, create)
                    .map_err(|e| errno_to_error(e, path))?;
            }
            Component::ParentDir => {
                return Err(DurabilityError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("refusing parent traversal in {}", path.display()),
                )));
            }
            Component::Prefix(_) => {
                return Err(DurabilityError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unsupported path prefix in {}", path.display()),
                )));
            }
        }
    }
    Ok((current, file_name.to_os_string()))
}

#[cfg(not(unix))]
fn reject_symlinked_path(path: &Path) -> Result<(), DurabilityError> {
    let mut current = if path.is_absolute() {
        std::path::PathBuf::new()
    } else {
        std::path::PathBuf::from(".")
    };
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(DurabilityError::SymlinkRefused(
                    current.display().to_string(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(DurabilityError::Io(error)),
        }
    }
    Ok(())
}

fn temp_name() -> OsString {
    let ts = uuid::Timestamp::now(uuid::NoContext);
    OsString::from(format!(".ardur-{}.tmp", uuid::Uuid::new_v7(ts).simple()))
}

/// Atomically replace (or create) a private, owner-only regular file at
/// `path` without following a symlinked parent or final component. On
/// success the file's prior contents (if any) are fully replaced; a crash at
/// any point before the internal rename leaves the original file (or no
/// file) untouched, never a partially written one.
pub fn write_atomic_no_follow(path: &Path, bytes: &[u8]) -> Result<(), DurabilityError> {
    #[cfg(not(unix))]
    {
        reject_symlinked_path(path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            reject_symlinked_path(parent)?;
        }
        let tmp = path.with_extension(format!(
            "{}.ardur-tmp",
            path.extension().and_then(|e| e.to_str()).unwrap_or("")
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        ensure_regular_file(&file, &tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        return Ok(());
    }

    #[cfg(unix)]
    {
        use rustix::fs::{AtFlags, Mode, OFlags, openat, renameat, unlinkat};

        let (parent, name) = open_parent_directory(path, true)?;

        // Reject a static symlink or non-regular destination up front. A
        // concurrent replacement after this check is still safe: renameat
        // replaces the directory entry relative to the already-open parent
        // descriptor and never follows it.
        match openat(
            &parent,
            &name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => ensure_regular_file(&File::from(fd), path)?,
            Err(error) if error == rustix::io::Errno::NOENT => {}
            Err(error) => return Err(errno_to_error(error, path)),
        }

        let tmp_name = temp_name();
        let fd = openat(
            &parent,
            &tmp_name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(|e| errno_to_error(e, path))?;
        let mut temp = File::from(fd);
        let result = (|| -> Result<(), DurabilityError> {
            ensure_regular_file(&temp, path)?;
            use std::os::unix::fs::PermissionsExt;
            temp.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            temp.write_all(bytes)?;
            temp.sync_all()?;
            renameat(&parent, &tmp_name, &parent, &name).map_err(|e| errno_to_error(e, path))?;
            parent.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = unlinkat(&parent, &tmp_name, AtFlags::empty());
        }
        result
    }
}

/// Atomically create a brand-new private, owner-only regular file: fails if
/// `path` already exists (mirrors `O_EXCL` semantics), but — unlike a bare
/// `O_EXCL` open followed by `write_all`/`sync_all` — a crash mid-write can
/// never leave a truncated file at `path`. The write lands in a temp file in
/// the same directory first; only a fully synced temp file is linked into
/// place, and the link step itself fails closed (`EEXIST`) if another writer
/// won the race.
pub fn create_new_atomic_no_follow(path: &Path, bytes: &[u8]) -> Result<(), DurabilityError> {
    #[cfg(not(unix))]
    {
        reject_symlinked_path(path)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            reject_symlinked_path(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        ensure_regular_file(&file, path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        return Ok(());
    }

    #[cfg(unix)]
    {
        use rustix::fs::{AtFlags, Mode, OFlags, linkat, openat, unlinkat};

        let (parent, name) = open_parent_directory(path, true)?;
        let tmp_name = temp_name();
        let fd = openat(
            &parent,
            &tmp_name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(|e| errno_to_error(e, path))?;
        let mut temp = File::from(fd);
        let result = (|| -> Result<(), DurabilityError> {
            ensure_regular_file(&temp, path)?;
            use std::os::unix::fs::PermissionsExt;
            temp.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            temp.write_all(bytes)?;
            temp.sync_all()?;
            linkat(&parent, &tmp_name, &parent, &name, AtFlags::empty())
                .map_err(|e| errno_to_error(e, path))?;
            parent.sync_all()?;
            Ok(())
        })();
        let _ = unlinkat(&parent, &tmp_name, AtFlags::empty());
        result
    }
}

#[cfg(unix)]
fn open_read_file_unix(path: &Path) -> Result<File, DurabilityError> {
    use rustix::fs::{Mode, OFlags, openat};

    let (parent, name) = open_parent_directory(path, false)?;
    let fd = openat(
        &parent,
        &name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|e| errno_to_error(e, path))?;
    let file = File::from(fd);
    ensure_regular_file(&file, path)?;
    Ok(file)
}

/// Read a regular file as bytes without following symlinks in its path.
pub fn read_no_follow(path: &Path) -> Result<Vec<u8>, DurabilityError> {
    #[cfg(unix)]
    let mut file = open_read_file_unix(path)?;
    #[cfg(not(unix))]
    let mut file = {
        reject_symlinked_path(path)?;
        let file = std::fs::OpenOptions::new().read(true).open(path)?;
        ensure_regular_file(&file, path)?;
        file
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Read a UTF-8 regular file without following symlinks in its path.
pub fn read_string_no_follow(path: &Path) -> Result<String, DurabilityError> {
    let bytes = read_no_follow(path)?;
    String::from_utf8(bytes).map_err(|error| {
        DurabilityError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not UTF-8: {error}", path.display()),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn read(path: &PathBuf) -> Vec<u8> {
        std::fs::read(path).expect("read back")
    }

    /// The trusted, no-follow walk requires every path component to be a
    /// real directory. On macOS `env::temp_dir()` resolves through a
    /// symlinked `/var` (-> `/private/var`), so tests that don't care about
    /// symlink rejection must canonicalize the tempdir root first — the
    /// same workaround `secure_io.rs`'s own tests use.
    fn root(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().canonicalize().expect("canonical tempdir root")
    }

    #[test]
    fn write_atomic_creates_and_replaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = root(&dir).join("state/receipt.pem");

        write_atomic_no_follow(&path, b"first").expect("initial write");
        assert_eq!(read(&path), b"first");

        write_atomic_no_follow(&path, b"second, longer payload").expect("replace");
        assert_eq!(read(&path), b"second, longer payload");
    }

    #[test]
    fn write_atomic_leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = root(&dir);
        let path = root.join("issuer.key");
        write_atomic_no_follow(&path, b"key-material").expect("write");

        let entries: Vec<_> = std::fs::read_dir(&root)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("issuer.key")]);
    }

    #[test]
    fn create_new_atomic_rejects_existing_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = root(&dir).join("issuer.key");

        create_new_atomic_no_follow(&path, b"first-key").expect("first create succeeds");
        assert_eq!(read(&path), b"first-key");

        let err = create_new_atomic_no_follow(&path, b"second-key")
            .expect_err("second create must fail closed like O_EXCL");
        assert!(matches!(err, DurabilityError::Io(_)));
        // The original key must be untouched by the failed second attempt.
        assert_eq!(read(&path), b"first-key");
    }

    #[test]
    fn create_new_atomic_leaves_no_temp_files_on_success_or_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = root(&dir);
        let path = root.join("receipt.pem");
        create_new_atomic_no_follow(&path, b"a").expect("create");
        let _ = create_new_atomic_no_follow(&path, b"b");

        let entries: Vec<_> = std::fs::read_dir(&root)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("receipt.pem")]);
    }

    #[test]
    fn read_round_trips_written_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = root(&dir).join("config.toml");
        write_atomic_no_follow(&path, b"model = \"x\"\n").expect("write");
        assert_eq!(
            read_string_no_follow(&path).expect("read"),
            "model = \"x\"\n"
        );
    }

    #[test]
    fn read_no_follow_reports_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = root(&dir).join("missing.jsonl");
        let err = read_no_follow(&path).expect_err("missing file");
        assert!(err.is_not_found(), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_rejects_symlinked_parent_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let root = root.path().canonicalize().expect("canonical root");
        let outside = outside.path().canonicalize().expect("canonical outside");
        symlink(&outside, root.join("state")).expect("parent symlink");

        let target = root.join("state/metadata.json");
        let err = write_atomic_no_follow(&target, b"forged")
            .expect_err("descriptor-relative atomic write must reject a symlinked parent");
        assert!(matches!(err, DurabilityError::SymlinkRefused(_)), "{err}");
        assert!(
            !outside.join("metadata.json").exists(),
            "the symlink target must remain untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_new_atomic_rejects_symlinked_parent_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let root = root.path().canonicalize().expect("canonical root");
        let outside = outside.path().canonicalize().expect("canonical outside");
        symlink(&outside, root.join("keys")).expect("parent symlink");

        let target = root.join("keys/issuer.key");
        let err = create_new_atomic_no_follow(&target, b"forged")
            .expect_err("must reject a symlinked parent");
        assert!(matches!(err, DurabilityError::SymlinkRefused(_)), "{err}");
        assert!(!outside.join("issuer.key").exists());
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_refuses_symlinked_final_component() {
        use std::os::unix::fs::symlink;

        let root_dir = tempfile::tempdir().expect("root tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let root_dir_path = root_dir.path().canonicalize().expect("canonical root");
        let outside = outside.path().canonicalize().expect("canonical outside");
        let victim = outside.join("victim.txt");
        std::fs::write(&victim, b"original").expect("victim file");
        let link = root_dir_path.join("config.toml");
        symlink(&victim, &link).expect("final-component symlink");

        let err = write_atomic_no_follow(&link, b"forged")
            .expect_err("must refuse a symlinked final component");
        assert!(
            matches!(
                err,
                DurabilityError::NotRegularFile(_) | DurabilityError::SymlinkRefused(_)
            ),
            "{err}"
        );
        assert_eq!(
            std::fs::read(&victim).expect("victim unchanged"),
            b"original"
        );
    }
}
