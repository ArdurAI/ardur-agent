#[cfg(unix)]
use crate::Profile;
use crate::{Error, StoredHome};
#[cfg(unix)]
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use zeroize::Zeroizing;
pub trait SecretStore {
    fn prepare(&self) -> Result<(), Error>;
    fn load(&self) -> Result<StoredHome, Error>;
    fn save(&self, home: &StoredHome) -> Result<(), Error>;
}
pub struct FileStore {
    path: PathBuf,
}
impl FileStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
}
pub fn default_config_dir() -> Result<PathBuf, Error> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA")
            .map(|v| PathBuf::from(v).join("ardur"))
            .ok_or(Error::Storage)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var_os("HOME").ok_or(Error::Storage)?;
        #[cfg(target_os = "macos")]
        {
            Ok(PathBuf::from(home).join("Library/Application Support/ardur"))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(home).join(".config"))
                .join("ardur"))
        }
    }
}
#[cfg(unix)]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DiskState<'a> {
    profile: Profile,
    #[serde(borrow)]
    private_key: &'a serde_json::value::RawValue,
}
#[cfg(unix)]
const MAX_KEY_BYTES: usize = 4096;
#[cfg(unix)]
fn read_key_bytes(raw: &str) -> Result<Zeroizing<Vec<u8>>, Error> {
    // Borrow the raw JSON so even a rejected legacy string never enters the
    // JSON string parser's unwiped scratch allocation.
    let inner = raw
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .ok_or(Error::Storage)?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(MAX_KEY_BYTES));
    if !inner.trim().is_empty() {
        for value in inner.split(',') {
            if bytes.len() == MAX_KEY_BYTES {
                return Err(Error::Storage);
            }
            let value = value.trim_matches([' ', '\n', '\r', '\t']);
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Error::Storage);
            }
            bytes.push(value.parse::<u8>().map_err(|_| Error::Storage)?);
        }
    }
    Ok(bytes)
}
#[cfg(unix)]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DiskStateRef<'a> {
    profile: &'a Profile,
    private_key: &'a [u8],
}
#[cfg(unix)]
mod unix {
    use super::*;
    use std::ffi::CString;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use std::path::Component;
    fn name(v: &std::ffi::OsStr) -> Result<CString, Error> {
        CString::new(v.as_bytes()).map_err(|_| Error::Storage)
    }
    fn open_at(parent: &File, n: &CString, flags: i32, mode: u32) -> Result<File, Error> {
        // SAFETY: parent is live, name is NUL-terminated, mode supplied for O_CREAT.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                n.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                mode as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(Error::Storage);
        }
        // SAFETY: openat returned a new owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
    fn checked(file: &File, dir: bool) -> Result<(), Error> {
        let m = file.metadata().map_err(|_| Error::Storage)?;
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        if m.uid() != uid
            || m.mode() & 0o7777 != if dir { 0o700 } else { 0o600 }
            || if dir {
                !m.is_dir()
            } else {
                !m.is_file() || m.nlink() != 1
            }
        {
            return Err(Error::Storage);
        }
        Ok(())
    }
    fn directory(path: &Path, create: bool) -> Result<File, Error> {
        if !path.is_absolute() {
            return Err(Error::Storage);
        }
        let mut dir = File::open("/").map_err(|_| Error::Storage)?;
        let components = path.components().collect::<Vec<_>>();
        for component in components {
            match component {
                Component::RootDir => continue,
                Component::Normal(c) => {
                    let n = name(c)?;
                    if create {
                        // SAFETY: live directory fd and NUL-terminated name. Existing dirs are checked on open.
                        let result = unsafe { libc::mkdirat(dir.as_raw_fd(), n.as_ptr(), 0o700) };
                        if result < 0
                            && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
                        {
                            return Err(Error::Storage);
                        }
                    }
                    dir = open_at(&dir, &n, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
                }
                _ => return Err(Error::Storage),
            }
        }
        checked(&dir, true)?;
        Ok(dir)
    }
    fn read_existing(dir: &File) -> Result<Option<File>, Error> {
        let n = CString::new("paired-home.json").expect("literal");
        // fstatat detects missing without following a link; openat revalidates the actual inode.
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: output pointer valid; no read of uninitialized data.
        let result = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                n.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result < 0 {
            return if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
                Ok(None)
            } else {
                Err(Error::Storage)
            };
        }
        let f = open_at(dir, &n, libc::O_RDONLY, 0)?;
        checked(&f, false)?;
        Ok(Some(f))
    }
    impl SecretStore for FileStore {
        fn prepare(&self) -> Result<(), Error> {
            let dir = directory(&self.path, true)?;
            read_existing(&dir)?;
            Ok(())
        }
        fn load(&self) -> Result<StoredHome, Error> {
            let dir = directory(&self.path, false)?;
            let file = read_existing(&dir)?.ok_or(Error::Storage)?;
            if file.metadata().map_err(|_| Error::Storage)?.len() > 65536 {
                return Err(Error::Storage);
            }
            // Fixed buffers also avoid freed copies of the encoded secret if
            // the file grows during a read or serialization exceeds its bound.
            let mut bytes = Zeroizing::new(vec![0; 65537]);
            let mut file = file;
            let mut len = 0;
            while len < bytes.len() {
                let read = file.read(&mut bytes[len..]).map_err(|_| Error::Storage)?;
                if read == 0 {
                    break;
                }
                len += read;
            }
            if len > 65536 {
                return Err(Error::Storage);
            }
            bytes.truncate(len);
            let disk: DiskState<'_> = serde_json::from_slice(&bytes).map_err(|_| Error::Storage)?;
            let mut key_bytes = read_key_bytes(disk.private_key.get())?;
            // Transfer the allocation into String rather than copying the PEM.
            let private_key = String::from_utf8(std::mem::take(&mut *key_bytes))
                .map(Zeroizing::new)
                .map_err(|error| {
                    let _wipe = Zeroizing::new(error.into_bytes());
                    Error::Storage
                })?;
            let home = StoredHome {
                profile: disk.profile,
                private_key,
            };
            home.validate()?;
            Ok(home)
        }
        fn save(&self, home: &StoredHome) -> Result<(), Error> {
            if home.private_key.len() > MAX_KEY_BYTES {
                return Err(Error::Storage);
            }
            home.validate()?;
            let dir = directory(&self.path, true)?;
            read_existing(&dir)?;
            let mut bytes = Zeroizing::new(vec![0; 65536]);
            let len = {
                // A slice writer cannot grow or leave reallocated encoded keys.
                let mut output = &mut bytes[..];
                serde_json::to_writer(
                    &mut output,
                    &DiskStateRef {
                        profile: &home.profile,
                        private_key: home.private_key.as_bytes(),
                    },
                )
                .map_err(|_| Error::Storage)?;
                65536 - output.len()
            };
            bytes.truncate(len);
            let temp = CString::new(format!(".pair-{}", home_protocol::nonce()))
                .map_err(|_| Error::Storage)?;
            let dest = CString::new("paired-home.json").expect("literal");
            let mut created = false;
            let result = (|| {
                let mut f = open_at(
                    &dir,
                    &temp,
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                    0o600,
                )?;
                created = true;
                checked(&f, false)?;
                f.write_all(&bytes).map_err(|_| Error::Storage)?;
                f.sync_all().map_err(|_| Error::Storage)?;
                read_existing(&dir)?;
                // SAFETY: both fds live, names valid; rename within the held private directory.
                if unsafe {
                    libc::renameat(
                        dir.as_raw_fd(),
                        temp.as_ptr(),
                        dir.as_raw_fd(),
                        dest.as_ptr(),
                    )
                } < 0
                {
                    return Err(Error::Storage);
                }
                dir.sync_all().map_err(|_| Error::Storage)
            })();
            if result.is_err() && created {
                // SAFETY: removes only the unique temporary name this save created.
                unsafe { libc::unlinkat(dir.as_raw_fd(), temp.as_ptr(), 0) };
            }
            result
        }
    }
}
#[cfg(not(unix))]
impl SecretStore for FileStore {
    // Fail closed until a current-user protected ACL backend is available.
    fn prepare(&self) -> Result<(), Error> {
        Err(Error::Storage)
    }
    fn load(&self) -> Result<StoredHome, Error> {
        Err(Error::Storage)
    }
    fn save(&self, _: &StoredHome) -> Result<(), Error> {
        Err(Error::Storage)
    }
}
