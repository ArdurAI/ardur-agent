//! Durable, owner-scoped JSON collection store for the operator surface (§9.7).
//!
//! A small generic store shared by the outbound endpoint registry and the
//! inbound trigger registry. The whole collection lives in one JSON file,
//! guarded by an in-process mutex; writes are atomic (temp file + rename).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::WebhookError;

/// A record that can live in a [`JsonCollectionStore`].
pub trait Identified {
    /// The record's stable id (the collection key).
    fn id(&self) -> &str;
}

/// A JSON-file-backed collection of [`Identified`] records.
#[derive(Debug)]
pub struct JsonCollectionStore<T> {
    path: PathBuf,
    lock: Mutex<()>,
    _marker: std::marker::PhantomData<T>,
}

impl<T> JsonCollectionStore<T>
where
    T: Identified + Serialize + DeserializeOwned + Clone,
{
    /// Open (or lazily create) a store at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
            _marker: std::marker::PhantomData,
        }
    }

    fn read_map(&self) -> Result<BTreeMap<String, T>, WebhookError> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) if text.trim().is_empty() => Ok(BTreeMap::new()),
            Ok(text) => {
                let records: Vec<T> =
                    serde_json::from_str(&text).map_err(|e| WebhookError::Serde(e.to_string()))?;
                Ok(records
                    .into_iter()
                    .map(|r| (r.id().to_string(), r))
                    .collect())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(WebhookError::Io(e.to_string())),
        }
    }

    fn write_map(&self, map: &BTreeMap<String, T>) -> Result<(), WebhookError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| WebhookError::Io(e.to_string()))?;
        }
        let records: Vec<&T> = map.values().collect();
        let json = serde_json::to_string_pretty(&records)
            .map_err(|e| WebhookError::Serde(e.to_string()))?;
        let tmp = temp_path(&self.path);
        std::fs::write(&tmp, json.as_bytes()).map_err(|e| WebhookError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| WebhookError::Io(e.to_string()))?;
        Ok(())
    }

    /// Load every record.
    pub fn load_all(&self) -> Result<Vec<T>, WebhookError> {
        let _guard = self.lock.lock().map_err(|_| poisoned())?;
        Ok(self.read_map()?.into_values().collect())
    }

    /// Fetch one record by id.
    pub fn get(&self, id: &str) -> Result<T, WebhookError> {
        let _guard = self.lock.lock().map_err(|_| poisoned())?;
        self.read_map()?
            .remove(id)
            .ok_or_else(|| WebhookError::HandlerNotFound(id.to_string()))
    }

    /// Insert or replace a record.
    pub fn upsert(&self, record: T) -> Result<(), WebhookError> {
        let _guard = self.lock.lock().map_err(|_| poisoned())?;
        let mut map = self.read_map()?;
        map.insert(record.id().to_string(), record);
        self.write_map(&map)
    }

    /// Remove a record; errors [`WebhookError::HandlerNotFound`] if absent.
    pub fn remove(&self, id: &str) -> Result<(), WebhookError> {
        let _guard = self.lock.lock().map_err(|_| poisoned())?;
        let mut map = self.read_map()?;
        if map.remove(id).is_none() {
            return Err(WebhookError::HandlerNotFound(id.to_string()));
        }
        self.write_map(&map)
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(
        path.file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("webhook-store")),
    );
    name.push(".tmp");
    path.with_file_name(name)
}

fn poisoned() -> WebhookError {
    WebhookError::Io("poisoned lock".to_string())
}
