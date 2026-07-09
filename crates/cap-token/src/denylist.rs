//! Revocation by Biscuit revocation identifier.

use std::collections::HashSet;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use parking_lot::Mutex;

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
/// If a lookup cannot reload the file, the implementation fails closed and
/// treats any non-empty token revocation-id set as revoked.
#[derive(Debug)]
pub struct FileDenyList {
    path: PathBuf,
    revoked: Mutex<HashSet<Vec<u8>>>,
}

impl FileDenyList {
    /// Open or create a file-backed deny list at `path`.
    ///
    /// # Errors
    /// Returns an I/O error if the parent directory or file cannot be created,
    /// or if an existing file contains a malformed hex line.
    pub fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let revoked = Self::load_ids(&path)?;
        Ok(Self {
            path,
            revoked: Mutex::new(revoked),
        })
    }

    /// The backing file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Revoke a single Biscuit revocation id and persist it durably.
    ///
    /// # Errors
    /// Returns an I/O error if the revocation cannot be appended and synced.
    pub fn revoke(&self, revocation_id: Vec<u8>) -> io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{}", hex::encode(&revocation_id))?;
        file.sync_all()?;
        self.revoked.lock().insert(revocation_id);
        Ok(())
    }

    /// Revoke a token by all of its block revocation ids and persist them
    /// durably.
    ///
    /// # Errors
    /// Returns an I/O error if any revocation id cannot be appended and synced.
    pub fn revoke_token(&self, token: &CapToken) -> io::Result<()> {
        for revocation_id in token.revocation_ids() {
            self.revoke(revocation_id)?;
        }
        Ok(())
    }

    fn reload(&self) -> io::Result<()> {
        let latest = Self::load_ids(&self.path)?;
        *self.revoked.lock() = latest;
        Ok(())
    }

    fn load_ids(path: &Path) -> io::Result<HashSet<Vec<u8>>> {
        let contents = std::fs::read_to_string(path)?;
        let mut revoked = HashSet::new();
        for (index, line) in contents.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let decoded = hex::decode(line).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid revocation id hex on line {}: {e}", index + 1),
                )
            })?;
            revoked.insert(decoded);
        }
        Ok(revoked)
    }
}

impl DenyList for FileDenyList {
    fn is_revoked(&self, revocation_ids: &[Vec<u8>]) -> bool {
        if revocation_ids.is_empty() {
            return false;
        }
        if self.reload().is_err() {
            return true;
        }
        let revoked = self.revoked.lock();
        revocation_ids.iter().any(|id| revoked.contains(id))
    }
}
