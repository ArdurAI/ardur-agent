use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

use crate::session::SessionId;

pub type CheckpointId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub session_id: SessionId,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub message_count: u64,
    pub token_count: u64,
    pub metadata: HashMap<String, String>,
}

impl Checkpoint {
    pub fn new(session_id: SessionId, title: &str, message_count: u64, token_count: u64) -> Self {
        Self {
            id: Uuid::now_v7().to_string(),
            session_id,
            title: title.to_string(),
            created_at: Utc::now(),
            message_count,
            token_count,
            metadata: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CheckpointManager {
    checkpoints: std::sync::Arc<std::sync::RwLock<HashMap<CheckpointId, Checkpoint>>>,
}

impl Default for CheckpointManager {
    fn default() -> Self {
        Self::new()
    }
}

impl CheckpointManager {
    pub fn new() -> Self {
        Self {
            checkpoints: std::sync::Arc::new(std::sync::RwLock::new(HashMap::new())),
        }
    }

    pub fn create(&self, checkpoint: Checkpoint) -> crate::error::Result<CheckpointId> {
        let mut checkpoints = self.checkpoints.write().map_err(|_| {
            crate::error::SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        let id = checkpoint.id.clone();
        checkpoints.insert(id.clone(), checkpoint);
        Ok(id)
    }

    pub fn get(&self, id: &CheckpointId) -> crate::error::Result<Checkpoint> {
        let checkpoints = self.checkpoints.read().map_err(|_| {
            crate::error::SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        checkpoints
            .get(id)
            .cloned()
            .ok_or_else(|| crate::error::SessionError::CheckpointNotFound(id.clone()))
    }

    pub fn list_for_session(
        &self,
        session_id: &SessionId,
    ) -> crate::error::Result<Vec<Checkpoint>> {
        let checkpoints = self.checkpoints.read().map_err(|_| {
            crate::error::SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(checkpoints
            .values()
            .filter(|c| c.session_id == *session_id)
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checkpoint_creation() {
        let cp = Checkpoint::new("session-1".to_string(), "Start", 0, 0);
        assert_eq!(cp.title, "Start");
        assert_eq!(cp.session_id, "session-1");
    }

    #[test]
    fn test_manager_create_and_get() {
        let manager = CheckpointManager::new();
        let cp = Checkpoint::new("session-1".to_string(), "Checkpoint 1", 10, 100);
        let id = manager.create(cp.clone()).unwrap();
        let retrieved = manager.get(&id).unwrap();
        assert_eq!(retrieved.title, "Checkpoint 1");
    }

    #[test]
    fn test_manager_list_for_session() {
        let manager = CheckpointManager::new();
        manager
            .create(Checkpoint::new("s1".to_string(), "CP1", 5, 50))
            .unwrap();
        manager
            .create(Checkpoint::new("s1".to_string(), "CP2", 10, 100))
            .unwrap();
        manager
            .create(Checkpoint::new("s2".to_string(), "CP3", 3, 30))
            .unwrap();

        let s1_cps = manager.list_for_session(&"s1".to_string()).unwrap();
        assert_eq!(s1_cps.len(), 2);
    }
}
