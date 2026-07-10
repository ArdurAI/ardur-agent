//! [`FileSessionJournal`] — an append-only JSONL backend.

#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::error::JournalError;
use crate::journal::SessionJournal;
use crate::types::{EntryId, JournalEntry};
use ardur_runtime::SessionId;

/// The mutable state guarded behind one lock: the append handle and the number
/// of entries written so far (the next entry's [`EntryId`]).
#[derive(Debug)]
struct FileState {
    handle: File,
    count: u64,
}

/// A file-backed [`SessionJournal`]: an append-only JSONL file at
/// `<base_dir>/sessions/<session_id>/journal.jsonl`, one serialized
/// [`JournalEntry`] per line.
///
/// [`append`](SessionJournal::append) writes the line then `fsync`s it, so a
/// returned [`EntryId`] always names a durably persisted entry. An entry's id
/// is its line number (0-based), so dropping the journal and reconstructing it
/// from the same path replays the same entries with the same ids. Concurrent
/// appends are serialized by a non-poisoning [`Mutex`] on the file handle.
#[derive(Debug)]
pub struct FileSessionJournal {
    session_id: SessionId,
    path: PathBuf,
    state: Mutex<FileState>,
}

#[cfg(not(unix))]
fn ensure_directory_not_symlink(path: &Path) -> Result<(), JournalError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(JournalError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("refusing symlinked journal directory {}", path.display()),
            )));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(JournalError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("journal path is not a directory: {}", path.display()),
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
        }
        Err(error) => return Err(JournalError::Io(error)),
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(JournalError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unsafe journal directory {}", path.display()),
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn open_or_create_directory_at(parent: &File, name: &str) -> Result<File, JournalError> {
    use rustix::fs::{Mode, OFlags, mkdirat, openat};

    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let open = || openat(parent, name, flags, Mode::empty());
    let descriptor = match open() {
        Ok(descriptor) => descriptor,
        Err(error) if error == rustix::io::Errno::NOENT => {
            match mkdirat(parent, name, Mode::from_bits_truncate(0o700)) {
                Ok(()) => {}
                Err(error) if error == rustix::io::Errno::EXIST => {}
                Err(error) => {
                    return Err(JournalError::Io(std::io::Error::from_raw_os_error(
                        error.raw_os_error(),
                    )));
                }
            }
            open().map_err(|error| {
                JournalError::Io(std::io::Error::from_raw_os_error(error.raw_os_error()))
            })?
        }
        Err(error) => {
            return Err(JournalError::Io(std::io::Error::from_raw_os_error(
                error.raw_os_error(),
            )));
        }
    };
    Ok(File::from(descriptor))
}

#[cfg(unix)]
fn open_journal_descriptor(base_dir: &Path, session_id: SessionId) -> Result<File, JournalError> {
    use rustix::fs::{Mode, OFlags, openat};

    let trusted_root = base_dir.parent().ok_or_else(|| {
        JournalError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "journal base has no trusted parent",
        ))
    })?;
    let base_name = base_dir.file_name().ok_or_else(|| {
        JournalError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "journal base has no file name",
        ))
    })?;
    fs::create_dir_all(trusted_root)?;
    let trusted = {
        use rustix::fs::{CWD, Mode, OFlags, openat};
        let fd = openat(
            CWD,
            trusted_root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| {
            JournalError::Io(std::io::Error::from_raw_os_error(error.raw_os_error()))
        })?;
        File::from(fd)
    };
    let base = open_or_create_directory_at(
        &trusted,
        base_name.to_str().ok_or_else(|| {
            JournalError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "journal base name is not UTF-8",
            ))
        })?,
    )?;
    let sessions = open_or_create_directory_at(&base, "sessions")?;
    let session = open_or_create_directory_at(&sessions, &session_id.0.to_string())?;
    let descriptor = openat(
        &session,
        "journal.jsonl",
        OFlags::RDWR | OFlags::APPEND | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|error| JournalError::Io(std::io::Error::from_raw_os_error(error.raw_os_error())))?;
    let file = File::from(descriptor);
    if !file.metadata()?.is_file() {
        return Err(JournalError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session journal is not a regular file",
        )));
    }
    use std::os::unix::fs::PermissionsExt as _;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn read_open_file(handle: &File) -> Result<String, JournalError> {
    let mut reader = handle.try_clone()?;
    reader.seek(std::io::SeekFrom::Start(0))?;
    let mut contents = String::new();
    reader.read_to_string(&mut contents)?;
    Ok(contents)
}

impl FileSessionJournal {
    /// Open (creating if absent) the journal for `session_id` under `base_dir`.
    ///
    /// Reconstructs the entry counter from any entries already on disk, so a
    /// fresh handle to an existing journal continues its id sequence rather than
    /// restarting it.
    ///
    /// # Errors
    ///
    /// Returns [`JournalError::Io`] if the session directory or file could not
    /// be created/opened, or [`JournalError::Serde`] if an existing line could
    /// not be counted because the file was malformed.
    pub fn new(base_dir: impl AsRef<Path>, session_id: SessionId) -> Result<Self, JournalError> {
        let base_dir = base_dir.as_ref();
        let path = base_dir
            .join("sessions")
            .join(session_id.0.to_string())
            .join("journal.jsonl");
        #[cfg(unix)]
        let mut handle = open_journal_descriptor(base_dir, session_id)?;
        #[cfg(not(unix))]
        let mut handle = {
            fs::create_dir_all(base_dir)?;
            let sessions_dir = base_dir.join("sessions");
            ensure_directory_not_symlink(&sessions_dir)?;
            let session_dir = sessions_dir.join(session_id.0.to_string());
            ensure_directory_not_symlink(&session_dir)?;
            if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
                return Err(JournalError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("refusing symlinked session journal {}", path.display()),
                )));
            }
            let mut options = OpenOptions::new();
            options.create(true).read(true).append(true);
            let handle = options.open(&path)?;
            if !handle.metadata()?.file_type().is_file() {
                return Err(JournalError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("session journal is not a regular file: {}", path.display()),
                )));
            }
            handle
        };
        let count = Self::recover_existing(&mut handle)?;
        Ok(Self {
            session_id,
            path,
            state: Mutex::new(FileState { handle, count }),
        })
    }

    /// The on-disk path of the journal file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read existing entries, repairing a single malformed unterminated tail
    /// left by a crash before the append handle is opened (0 if absent).
    fn recover_existing(handle: &mut File) -> Result<u64, JournalError> {
        let contents = read_open_file(handle)?;
        let (entries, torn_tail_start) = Self::parse_entries(&contents)?;
        if let Some(offset) = torn_tail_start {
            tracing::warn!(
                truncate_at = offset,
                "dropping torn trailing session-journal line"
            );
            handle.set_len(offset as u64)?;
            handle.sync_all()?;
        }
        Ok(entries.len() as u64)
    }

    fn parse_entries(contents: &str) -> Result<(Vec<JournalEntry>, Option<usize>), JournalError> {
        let mut entries = Vec::new();
        let mut offset = 0;
        for (line_index, segment) in contents.split_inclusive('\n').enumerate() {
            let segment_start = offset;
            offset += segment.len();
            let line = segment.strip_suffix('\n').unwrap_or(segment);
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(line) {
                Ok(entry) => entries.push(entry),
                Err(err) if !segment.ends_with('\n') && offset == contents.len() => {
                    tracing::warn!(
                        line_index,
                        error = %err,
                        "ignoring malformed trailing session-journal line"
                    );
                    return Ok((entries, Some(segment_start)));
                }
                Err(err) => return Err(JournalError::Serde(err)),
            }
        }
        Ok((entries, None))
    }

    /// Read and deserialize every entry, under the append lock so we never see
    /// a partially written line.
    fn read_all(&self) -> Result<Vec<JournalEntry>, JournalError> {
        let mut state = self.state.lock();
        let contents = read_open_file(&state.handle)?;
        let (entries, torn_tail_start) = Self::parse_entries(&contents)?;
        if let Some(offset) = torn_tail_start {
            tracing::warn!(
                path = %self.path.display(),
                truncate_at = offset,
                "dropping torn trailing session-journal line"
            );
            state.handle.set_len(offset as u64)?;
            state.handle.sync_all()?;
            state.count = entries.len() as u64;
        }
        Ok(entries)
    }
}

#[async_trait]
impl SessionJournal for FileSessionJournal {
    async fn append(&self, entry: JournalEntry) -> Result<EntryId, JournalError> {
        // Serialize before taking the lock so a malformed entry never holds it.
        let mut line = serde_json::to_string(&entry)?;
        line.push('\n');

        let mut state = self.state.lock();
        state.handle.write_all(line.as_bytes())?;
        state.handle.flush()?;
        // Write-then-fsync: a returned EntryId names a durably persisted entry.
        state.handle.sync_all()?;
        let id = EntryId::new(state.count);
        state.count += 1;
        Ok(id)
    }

    async fn len(&self) -> Result<u64, JournalError> {
        Ok(self.state.lock().count)
    }

    async fn is_empty(&self) -> Result<bool, JournalError> {
        Ok(self.state.lock().count == 0)
    }

    async fn truncate(&self, len: u64) -> Result<(), JournalError> {
        let mut state = self.state.lock();
        if len > state.count {
            return Err(JournalError::EntryNotFound(EntryId(len)));
        }
        if len == state.count {
            return Ok(());
        }
        state.handle.flush()?;
        state.handle.sync_all()?;
        let contents = read_open_file(&state.handle)?;
        let mut lines = contents
            .lines()
            .filter(|line| !line.is_empty())
            .take(len as usize)
            .collect::<Vec<_>>()
            .join("\n");
        if !lines.is_empty() {
            lines.push('\n');
        }
        state.handle.set_len(0)?;
        state.handle.write_all(lines.as_bytes())?;
        state.handle.flush()?;
        state.handle.sync_all()?;
        state.count = len;
        Ok(())
    }

    async fn replay(&self, session_id: SessionId) -> Result<Vec<JournalEntry>, JournalError> {
        if session_id != self.session_id {
            return Err(JournalError::SessionNotFound(session_id));
        }
        self.read_all()
    }

    async fn replay_from(
        &self,
        session_id: SessionId,
        from: EntryId,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        if session_id != self.session_id {
            return Err(JournalError::SessionNotFound(session_id));
        }
        let entries = self.read_all()?;
        // `from` is exclusive, so the first returned entry is at `from + 1`.
        let start = from.value().saturating_add(1) as usize;
        if start > entries.len() {
            return Err(JournalError::EntryNotFound(from));
        }
        Ok(entries[start..].to_vec())
    }

    async fn close(&self) -> Result<(), JournalError> {
        let mut state = self.state.lock();
        state.handle.flush()?;
        state.handle.sync_all()?;
        Ok(())
    }

    fn session_id(&self) -> &SessionId {
        &self.session_id
    }
}
