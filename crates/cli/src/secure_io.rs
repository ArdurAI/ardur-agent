//! Race-resistant private-file helpers for CLI state and exports.
//!
//! On Unix, every parent component is opened descriptor-relatively with
//! `O_NOFOLLOW | O_DIRECTORY`; the final component is then opened relative to
//! the verified parent. This prevents both final-component and parent-directory
//! symlink substitution. Portable platforms retain explicit symlink checks.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Component, Path};

#[cfg(unix)]
use std::ffi::{OsStr, OsString};

fn ensure_regular_file(file: &File, path: &Path) -> io::Result<()> {
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing non-regular file {}", path.display()),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn errno_to_io(error: rustix::io::Errno) -> io::Error {
    if error == rustix::io::Errno::LOOP {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing symlink in trusted state path",
        )
    } else {
        io::Error::from_raw_os_error(error.raw_os_error())
    }
}

#[cfg(unix)]
fn open_directory_at(parent: &File, name: &OsStr, create: bool) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags, mkdirat, openat};

    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match openat(parent, name, flags, Mode::empty()) {
        Ok(fd) => Ok(fd.into()),
        Err(error) if create && error == rustix::io::Errno::NOENT => {
            match mkdirat(parent, name, Mode::from_bits_truncate(0o700)) {
                Ok(()) => {}
                Err(error) if error == rustix::io::Errno::EXIST => {}
                Err(error) => return Err(errno_to_io(error)),
            }
            openat(parent, name, flags, Mode::empty())
                .map(File::from)
                .map_err(errno_to_io)
        }
        Err(error) => Err(errno_to_io(error)),
    }
}

#[cfg(unix)]
fn open_parent_directory(path: &Path, create: bool) -> io::Result<(File, OsString)> {
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path has no file name: {}", path.display()),
        )
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
                current = open_directory_at(&current, name, create)?;
            }
            Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("refusing parent traversal in {}", path.display()),
                ));
            }
            Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unsupported path prefix in {}", path.display()),
                ));
            }
        }
    }
    Ok((current, file_name.to_os_string()))
}

#[cfg(unix)]
fn open_private_file_unix(path: &Path, create_new: bool) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags, openat};

    let (parent, name) = open_parent_directory(path, true)?;
    let mut flags =
        OFlags::WRONLY | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    if create_new {
        flags |= OFlags::EXCL;
    } else {
        flags |= OFlags::TRUNC;
    }
    let fd = openat(&parent, &name, flags, Mode::from_bits_truncate(0o600)).map_err(errno_to_io)?;
    let file = File::from(fd);
    ensure_regular_file(&file, path)?;
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

#[cfg(unix)]
fn open_read_file_unix(path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags, openat};

    let (parent, name) = open_parent_directory(path, false)?;
    let fd = openat(
        &parent,
        &name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(errno_to_io)?;
    let file = File::from(fd);
    ensure_regular_file(&file, path)?;
    Ok(file)
}

#[cfg(not(unix))]
fn reject_symlinked_path(path: &Path) -> io::Result<()> {
    let mut current = if path.is_absolute() {
        std::path::PathBuf::new()
    } else {
        std::path::PathBuf::from(".")
    };
    for component in path.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("refusing symlink path {}", current.display()),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn open_private_file_portable(path: &Path, create_new: bool) -> io::Result<File> {
    use std::fs::OpenOptions;

    reject_symlinked_path(path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        reject_symlinked_path(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    let file = options.open(path)?;
    ensure_regular_file(&file, path)?;
    Ok(file)
}

#[cfg(unix)]
fn write_open_file(mut file: File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_open_file(mut file: File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()
}

/// Create a new owner-only regular file, refusing symlinks in every path component.
pub fn create_private_file_no_follow(path: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    let file = open_private_file_unix(path, true)?;
    #[cfg(not(unix))]
    let file = open_private_file_portable(path, true)?;
    write_open_file(file, bytes)
}

/// Replace an owner-only regular file in place without following symlinks.
pub fn write_private_file_no_follow(path: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    let file = open_private_file_unix(path, false)?;
    #[cfg(not(unix))]
    let file = open_private_file_portable(path, false)?;
    write_open_file(file, bytes)
}

/// Atomically replace a private file without resolving either its parent or
/// final component through a symlink.
pub fn write_private_file_atomic_no_follow(path: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(not(unix))]
    {
        return write_private_file_no_follow(path, bytes);
    }

    #[cfg(unix)]
    {
        use rustix::fs::{AtFlags, Mode, OFlags, openat, renameat, unlinkat};

        let (parent, name) = open_parent_directory(path, true)?;

        // Reject a static symlink or non-regular destination. A concurrent
        // replacement after this check is still safe: renameat replaces the
        // directory entry relative to the already-open parent and never follows it.
        match openat(
            &parent,
            &name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => ensure_regular_file(&File::from(fd), path)?,
            Err(error) if error == rustix::io::Errno::NOENT => {}
            Err(error) => return Err(errno_to_io(error)),
        }

        let temp_name = OsString::from(format!(".ardur-{}.tmp", uuid::Uuid::new_v4().simple()));
        let fd = openat(
            &parent,
            &temp_name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )
        .map_err(errno_to_io)?;
        let mut temp = File::from(fd);
        let result = (|| {
            ensure_regular_file(&temp, path)?;
            use std::os::unix::fs::PermissionsExt;
            temp.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            temp.write_all(bytes)?;
            temp.sync_all()?;
            renameat(&parent, &temp_name, &parent, &name).map_err(errno_to_io)?;
            parent.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = unlinkat(&parent, &temp_name, AtFlags::empty());
        }
        result
    }
}

#[cfg(unix)]
fn open_directory_path_unix(path: &Path) -> io::Result<File> {
    let (parent, name) = open_parent_directory(path, false)?;
    open_directory_at(&parent, &name, false)
}

/// Enumerate a directory without following symlinks in any path component.
pub fn list_directory_names_no_follow(path: &Path) -> io::Result<Vec<std::ffi::OsString>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let directory = open_directory_path_unix(path)?;
        let mut stream = rustix::fs::Dir::read_from(&directory).map_err(errno_to_io)?;
        let mut names = Vec::new();
        while let Some(entry) = stream.read() {
            let entry = entry.map_err(errno_to_io)?;
            let bytes = entry.file_name().to_bytes();
            if bytes != b"." && bytes != b".." {
                names.push(OsString::from_vec(bytes.to_vec()));
            }
        }
        Ok(names)
    }

    #[cfg(not(unix))]
    {
        reject_symlinked_path(path)?;
        std::fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect()
    }
}

/// Return a directory's modification time without following symlinked components.
pub fn directory_modified_no_follow(path: &Path) -> io::Result<std::time::SystemTime> {
    #[cfg(unix)]
    let directory = open_directory_path_unix(path)?;
    #[cfg(not(unix))]
    let directory = {
        reject_symlinked_path(path)?;
        File::open(path)?
    };
    directory.metadata()?.modified()
}

#[cfg(unix)]
fn remove_open_directory_contents(directory: &File) -> io::Result<()> {
    use rustix::fs::{AtFlags, Mode, OFlags, openat, unlinkat};

    let mut stream = rustix::fs::Dir::read_from(directory).map_err(errno_to_io)?;
    while let Some(entry) = stream.read() {
        let entry = entry.map_err(errno_to_io)?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        match openat(
            directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => {
                let child = File::from(fd);
                remove_open_directory_contents(&child)?;
                unlinkat(directory, name, AtFlags::REMOVEDIR).map_err(errno_to_io)?;
            }
            Err(error)
                if error == rustix::io::Errno::NOTDIR || error == rustix::io::Errno::LOOP =>
            {
                unlinkat(directory, name, AtFlags::empty()).map_err(errno_to_io)?;
            }
            Err(error) => return Err(errno_to_io(error)),
        }
    }
    Ok(())
}

/// Recursively remove a directory tree without following symlinked parents or
/// children. Symlink entries inside the tree are unlinked, never traversed.
pub fn remove_directory_tree_no_follow(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use rustix::fs::{AtFlags, Mode, OFlags, openat, unlinkat};
        let (parent, name) = open_parent_directory(path, false)?;
        let fd = openat(
            &parent,
            &name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(errno_to_io)?;
        let directory = File::from(fd);
        remove_open_directory_contents(&directory)?;
        unlinkat(&parent, &name, AtFlags::REMOVEDIR).map_err(errno_to_io)?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        reject_symlinked_path(path)?;
        std::fs::remove_dir_all(path)
    }
}

/// Read a regular file as bytes without following symlinks in its path.
pub fn read_file_no_follow(path: &Path) -> io::Result<Vec<u8>> {
    #[cfg(unix)]
    let mut file = open_read_file_unix(path)?;
    #[cfg(not(unix))]
    let mut file = {
        use std::fs::OpenOptions;
        reject_symlinked_path(path)?;
        let file = OpenOptions::new().read(true).open(path)?;
        ensure_regular_file(&file, path)?;
        file
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Read a UTF-8 regular file without following symlinks in its path.
pub fn read_string_no_follow(path: &Path) -> io::Result<String> {
    String::from_utf8(read_file_no_follow(path)?).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not UTF-8: {error}", path.display()),
        )
    })
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn atomic_write_rejects_symlinked_parent_directory() {
        let root = tempfile::tempdir().expect("root tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let root = root.path().canonicalize().expect("canonical root");
        let outside = outside.path().canonicalize().expect("canonical outside");
        symlink(&outside, root.join("state")).expect("parent symlink");

        let target = root.join("state/metadata.json");
        assert!(
            write_private_file_atomic_no_follow(&target, b"forged").is_err(),
            "descriptor-relative atomic write must reject a symlinked parent"
        );
        assert!(
            !outside.join("metadata.json").exists(),
            "the symlink target must remain untouched"
        );
    }

    #[test]
    fn recursive_remove_unlinks_nothing_through_symlinked_parent() {
        let root = tempfile::tempdir().expect("root tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let root = root.path().canonicalize().expect("canonical root");
        let outside = outside.path().canonicalize().expect("canonical outside");
        let victim = outside.join("victim");
        std::fs::create_dir(&victim).expect("victim dir");
        std::fs::write(victim.join("sentinel"), b"keep").expect("sentinel");
        symlink(&outside, root.join("sessions")).expect("parent symlink");

        assert!(
            remove_directory_tree_no_follow(&root.join("sessions/victim")).is_err(),
            "recursive removal must reject a symlinked parent"
        );
        assert!(
            victim.join("sentinel").is_file(),
            "the symlink target tree must remain untouched"
        );
    }
}
