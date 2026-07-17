//! Durable, owner-scoped cron store (§9.4).
//!
//! The UI is stateless with respect to cron data: it reads from and mutates the
//! same store the scheduler substrate uses. This module provides a small
//! [`CronStore`] trait with a JSON file-backed implementation ([`FileCronStore`])
//! and an in-memory one ([`InMemoryCronStore`]) for tests.
//!
//! Writes are atomic (temp file + rename) so a crash mid-write never leaves a
//! half-written store.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::domain::{CronRecord, RunSummary};
use crate::error::{CronUiError, Result};

/// Read/mutate access to the durable cron collection.
pub trait CronStore: Send + Sync {
    /// Load every stored cron.
    fn load_all(&self) -> Result<Vec<CronRecord>>;
    /// Fetch one cron by id.
    fn get(&self, id: &str) -> Result<CronRecord>;
    /// Insert or replace a cron.
    fn upsert(&self, record: CronRecord) -> Result<()>;
    /// Remove a cron; errors [`CronUiError::NotFound`] if absent.
    fn remove(&self, id: &str) -> Result<()>;
    /// Append a run summary to a cron's history (bounded ring) and bump its
    /// run count.
    fn record_run(&self, id: &str, run: RunSummary) -> Result<()>;
}

/// Maximum retained runs per cron. Older runs are dropped from the head.
pub const MAX_RUN_HISTORY: usize = 100;

fn apply_run(record: &mut CronRecord, run: RunSummary) {
    record.run_history.push(run);
    if record.run_history.len() > MAX_RUN_HISTORY {
        let overflow = record.run_history.len() - MAX_RUN_HISTORY;
        record.run_history.drain(0..overflow);
    }
    record.run_count = record.run_count.saturating_add(1);
}

/// A JSON-file-backed store. The whole collection lives in one file, guarded by
/// an in-process mutex; writes are atomic via a temp file + rename.
#[derive(Debug)]
pub struct FileCronStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileCronStore {
    /// Open (or lazily create) a store at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    fn read_map(&self) -> Result<BTreeMap<String, CronRecord>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) if text.trim().is_empty() => Ok(BTreeMap::new()),
            Ok(text) => {
                let records: Vec<CronRecord> =
                    serde_json::from_str(&text).map_err(|e| CronUiError::Serde(e.to_string()))?;
                Ok(records.into_iter().map(|r| (r.id.clone(), r)).collect())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(CronUiError::Io(e.to_string())),
        }
    }

    fn write_map(&self, map: &BTreeMap<String, CronRecord>) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CronUiError::Io(e.to_string()))?;
        }
        let records: Vec<&CronRecord> = map.values().collect();
        let json = serde_json::to_string_pretty(&records)
            .map_err(|e| CronUiError::Serde(e.to_string()))?;
        let tmp = temp_path(&self.path);
        std::fs::write(&tmp, json.as_bytes()).map_err(|e| CronUiError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| CronUiError::Io(e.to_string()))?;
        Ok(())
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(
        path.file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("cron-store")),
    );
    name.push(".tmp");
    path.with_file_name(name)
}

impl CronStore for FileCronStore {
    fn load_all(&self) -> Result<Vec<CronRecord>> {
        let _guard = self.lock.lock().map_err(|_| poisoned())?;
        Ok(self.read_map()?.into_values().collect())
    }

    fn get(&self, id: &str) -> Result<CronRecord> {
        let _guard = self.lock.lock().map_err(|_| poisoned())?;
        self.read_map()?
            .remove(id)
            .ok_or_else(|| CronUiError::NotFound(id.to_string()))
    }

    fn upsert(&self, record: CronRecord) -> Result<()> {
        let _guard = self.lock.lock().map_err(|_| poisoned())?;
        let mut map = self.read_map()?;
        map.insert(record.id.clone(), record);
        self.write_map(&map)
    }

    fn remove(&self, id: &str) -> Result<()> {
        let _guard = self.lock.lock().map_err(|_| poisoned())?;
        let mut map = self.read_map()?;
        if map.remove(id).is_none() {
            return Err(CronUiError::NotFound(id.to_string()));
        }
        self.write_map(&map)
    }

    fn record_run(&self, id: &str, run: RunSummary) -> Result<()> {
        let _guard = self.lock.lock().map_err(|_| poisoned())?;
        let mut map = self.read_map()?;
        let record = map
            .get_mut(id)
            .ok_or_else(|| CronUiError::NotFound(id.to_string()))?;
        apply_run(record, run);
        self.write_map(&map)
    }
}

/// An in-memory store for tests and embedded use.
#[derive(Debug, Default)]
pub struct InMemoryCronStore {
    map: Mutex<BTreeMap<String, CronRecord>>,
}

impl InMemoryCronStore {
    /// Create an empty in-memory store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl CronStore for InMemoryCronStore {
    fn load_all(&self) -> Result<Vec<CronRecord>> {
        Ok(self
            .map
            .lock()
            .map_err(|_| poisoned())?
            .values()
            .cloned()
            .collect())
    }

    fn get(&self, id: &str) -> Result<CronRecord> {
        self.map
            .lock()
            .map_err(|_| poisoned())?
            .get(id)
            .cloned()
            .ok_or_else(|| CronUiError::NotFound(id.to_string()))
    }

    fn upsert(&self, record: CronRecord) -> Result<()> {
        self.map
            .lock()
            .map_err(|_| poisoned())?
            .insert(record.id.clone(), record);
        Ok(())
    }

    fn remove(&self, id: &str) -> Result<()> {
        if self
            .map
            .lock()
            .map_err(|_| poisoned())?
            .remove(id)
            .is_none()
        {
            return Err(CronUiError::NotFound(id.to_string()));
        }
        Ok(())
    }

    fn record_run(&self, id: &str, run: RunSummary) -> Result<()> {
        let mut map = self.map.lock().map_err(|_| poisoned())?;
        let record = map
            .get_mut(id)
            .ok_or_else(|| CronUiError::NotFound(id.to_string()))?;
        apply_run(record, run);
        Ok(())
    }
}

fn poisoned() -> CronUiError {
    CronUiError::Io("poisoned lock".to_string())
}
