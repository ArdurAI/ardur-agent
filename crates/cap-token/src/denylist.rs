//! Revocation by Biscuit revocation identifier.

use std::collections::HashSet;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

use crate::types::CapToken;

/// A revocation oracle consulted by the verifier. A token is rejected if any of
/// its block revocation ids is denied — revoking a parent therefore revokes
/// every token attenuated from it, since the child still carries the parent's
/// block (and thus its revocation id).
pub trait DenyList {
    /// Whether any of `revocation_ids` (a token's per-block ids) is revoked.
    fn is_revoked(&self, revocation_ids: &[Vec<u8>]) -> bool;
}

/// An in-memory [`DenyList`] backed by a hash set of revocation ids.
///
/// Use [`FileDenyList`] when revocations must survive restart or propagate to
/// other verifier instances.
#[derive(Debug, Default, Clone)]
pub struct HashSetDenyList {
    revoked: HashSet<Vec<u8>>,
}

impl HashSetDenyList {
    /// An empty deny list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Revoke a single Biscuit revocation id.
    pub fn revoke(&mut self, revocation_id: Vec<u8>) {
        self.revoked.insert(revocation_id);
    }

    /// Revoke a token by all of its block revocation ids.
    pub fn revoke_token(&mut self, token: &CapToken) {
        self.revoked.extend(token.revocation_ids());
    }
}

impl DenyList for HashSetDenyList {
    fn is_revoked(&self, revocation_ids: &[Vec<u8>]) -> bool {
        revocation_ids.iter().any(|id| self.revoked.contains(id))
    }
}

/// A newline-delimited, hex-encoded, file-backed deny list.
///
/// `FileDenyList` persists every revoked Biscuit revocation id to disk and
/// reloads the file on each verifier lookup. That makes revocation visible to
/// independently constructed verifier instances that share the same path
/// (ARD-482), not just to the process that called [`revoke`](Self::revoke).
///
/// If a lookup cannot lock, read, or validate the file, the implementation fails
/// closed and treats any non-empty token revocation-id set as revoked. Records
/// must be complete hex lines containing an Ed25519 signature or a P-256 DER
/// signature. LF and CRLF are accepted; blank lines and torn tails are errors.
///
/// Operations use advisory file locks across threads and processes: readers
/// share a lock, and each append holds an exclusive lock through `sync_all`.
/// All participants must follow this protocol on a filesystem supporting these
/// locks. The path must not be unlinked, replaced, or truncated while in use;
/// this is not an authenticated log and does not protect against external edits.
///
/// Success means the complete record was written and the file's `sync_all`
/// returned successfully, not a guarantee against every storage/power failure.
/// Parent-directory creation is not synced. Only `open` may create the file;
/// lookups and revocations fail closed if it subsequently goes missing.
#[derive(Debug)]
pub struct FileDenyList {
    path: PathBuf,
}

impl FileDenyList {
    /// Open or create a file-backed deny list at `path`.
    ///
    /// # Errors
    /// Returns an I/O error if the parent directory or file cannot be created,
    /// locked, or read, or if an existing file contains malformed records.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)?;
        file.lock_shared()?;
        Self::read_ids(&mut file)?;
        Ok(Self { path })
    }

    /// The backing file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Revoke a single Biscuit revocation id, append it, and sync the file.
    ///
    /// # Errors
    /// Returns `InvalidInput` for a malformed id, `InvalidData` for malformed
    /// existing records, or an I/O error if the file cannot be opened, locked,
    /// read, appended, or synced. Errors may leave an unacknowledged complete
    /// record or a torn tail; a torn tail requires operator repair before reuse.
    pub fn revoke(&self, revocation_id: Vec<u8>) -> io::Result<()> {
        if !valid_revocation_id(&revocation_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid Biscuit revocation id framing",
            ));
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&self.path)?;
        // Independently open on each operation: cloned handles can share a lock.
        // Keep this file (and its exclusive lock) alive through validation,
        // every write, and sync. Dropping it releases the lock on errors too.
        file.lock()?;
        Self::read_ids(&mut file)?;
        let mut record = hex::encode(&revocation_id);
        record.push('\n');
        file.write_all(record.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    /// Revoke a token by appending and syncing each of its block revocation ids.
    /// This is not an all-or-nothing transaction: earlier ids remain revoked if
    /// a later append fails.
    ///
    /// # Errors
    /// Returns an error from [`revoke`](Self::revoke) for any block id.
    pub fn revoke_token(&self, token: &CapToken) -> io::Result<()> {
        for revocation_id in token.revocation_ids() {
            self.revoke(revocation_id)?;
        }
        Ok(())
    }

    fn load_ids(path: &Path) -> io::Result<HashSet<Vec<u8>>> {
        let mut file = std::fs::File::open(path)?;
        file.lock_shared()?;
        Self::read_ids(&mut file)
    }

    // The caller must hold a shared or exclusive lock on this exact handle.
    fn read_ids(file: &mut std::fs::File) -> io::Result<HashSet<Vec<u8>>> {
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        if !contents.is_empty() && !contents.ends_with('\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unterminated revocation record",
            ));
        }
        let mut revoked = HashSet::new();
        for (index, line) in contents.split_terminator('\n').enumerate() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let decoded = hex::decode(line).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid revocation id hex on line {}: {e}", index + 1),
                )
            })?;
            if !valid_revocation_id(&decoded) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid revocation id framing on line {}", index + 1),
                ));
            }
            revoked.insert(decoded);
        }
        Ok(revoked)
    }
}

// biscuit-auth 6 returns each block's signature bytes as its revocation id:
// Ed25519 is 64 bytes; P-256 is DER, not a fixed-width signature. Check only
// framing here, not cryptographic validity. DER holds two positive, minimally
// encoded integers of at most 32 bytes (plus an optional sign-padding byte).
fn valid_revocation_id(id: &[u8]) -> bool {
    if id.len() == 64 {
        return true;
    }
    if !(8..=72).contains(&id.len()) || id[0] != 0x30 || usize::from(id[1]) != id.len() - 2 {
        return false;
    }
    let mut rest = &id[2..];
    for _ in 0..2 {
        if rest.len() < 3 || rest[0] != 0x02 {
            return false;
        }
        let len = usize::from(rest[1]);
        if !(1..=33).contains(&len) || rest.len() < 2 + len {
            return false;
        }
        let integer = &rest[2..2 + len];
        if integer[0] & 0x80 != 0
            || (integer[0] == 0 && (len == 1 || integer[1] & 0x80 == 0))
            || (len == 33 && integer[0] != 0)
        {
            return false;
        }
        rest = &rest[2 + len..];
    }
    rest.is_empty()
}

impl DenyList for FileDenyList {
    fn is_revoked(&self, revocation_ids: &[Vec<u8>]) -> bool {
        if revocation_ids.is_empty() {
            return false;
        }
        match Self::load_ids(&self.path) {
            Ok(revoked) => revocation_ids.iter().any(|id| revoked.contains(id)),
            Err(_) => true,
        }
    }
}
