use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::commitment::{Commitment, CommitmentId, CommitmentStatus, Priority};
use crate::error::{CommitmentError, Result};

#[derive(Debug, Clone)]
pub struct CommitmentRegistry {
    commitments: Arc<RwLock<HashMap<CommitmentId, Commitment>>>,
}

impl Default for CommitmentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CommitmentRegistry {
    pub fn new() -> Self {
        Self {
            commitments: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn create(&self, commitment: Commitment) -> Result<CommitmentId> {
        let mut commitments = self.commitments.write().map_err(|_| {
            CommitmentError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        let id = commitment.id.clone();
        commitments.insert(id.clone(), commitment);
        Ok(id)
    }

    pub fn get(&self, id: &CommitmentId) -> Result<Commitment> {
        let commitments = self.commitments.read().map_err(|_| {
            CommitmentError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        commitments
            .get(id)
            .cloned()
            .ok_or_else(|| CommitmentError::NotFound(id.clone()))
    }

    pub fn list(&self) -> Result<Vec<Commitment>> {
        let commitments = self.commitments.read().map_err(|_| {
            CommitmentError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(commitments.values().cloned().collect())
    }

    pub fn list_by_status(&self, status: CommitmentStatus) -> Result<Vec<Commitment>> {
        let commitments = self.commitments.read().map_err(|_| {
            CommitmentError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(commitments
            .values()
            .filter(|c| c.status == status)
            .cloned()
            .collect())
    }

    pub fn list_by_priority(&self, priority: Priority) -> Result<Vec<Commitment>> {
        let commitments = self.commitments.read().map_err(|_| {
            CommitmentError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(commitments
            .values()
            .filter(|c| c.priority == priority)
            .cloned()
            .collect())
    }

    pub fn update(&self, commitment: Commitment) -> Result<()> {
        let mut commitments = self.commitments.write().map_err(|_| {
            CommitmentError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        if !commitments.contains_key(&commitment.id) {
            return Err(CommitmentError::NotFound(commitment.id.clone()));
        }
        commitments.insert(commitment.id.clone(), commitment);
        Ok(())
    }

    pub fn remove(&self, id: &CommitmentId) -> Result<()> {
        let mut commitments = self.commitments.write().map_err(|_| {
            CommitmentError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        commitments
            .remove(id)
            .ok_or_else(|| CommitmentError::NotFound(id.clone()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_create_and_get() {
        let registry = CommitmentRegistry::new();
        let c = Commitment::new("Test", "Desc", "gnani");
        let id = registry.create(c.clone()).unwrap();
        let retrieved = registry.get(&id).unwrap();
        assert_eq!(retrieved.title, "Test");
    }

    #[test]
    fn test_registry_list_by_status() {
        let registry = CommitmentRegistry::new();
        let mut c1 = Commitment::new("Active", "Desc", "gnani");
        c1.start();
        let c2 = Commitment::new("Declared", "Desc", "gnani");
        registry.create(c1).unwrap();
        registry.create(c2).unwrap();

        let in_progress = registry.list_by_status(CommitmentStatus::InProgress).unwrap();
        assert_eq!(in_progress.len(), 1);
        assert_eq!(in_progress[0].title, "Active");
    }

    #[test]
    fn test_registry_list_by_priority() {
        let registry = CommitmentRegistry::new();
        let c1 = Commitment::new("High", "Desc", "gnani").with_priority(Priority::High);
        let c2 = Commitment::new("Low", "Desc", "gnani").with_priority(Priority::Low);
        registry.create(c1).unwrap();
        registry.create(c2).unwrap();

        let high = registry.list_by_priority(Priority::High).unwrap();
        assert_eq!(high.len(), 1);
        assert_eq!(high[0].title, "High");
    }

    #[test]
    fn test_registry_remove() {
        let registry = CommitmentRegistry::new();
        let c = Commitment::new("Remove", "Desc", "gnani");
        let id = registry.create(c).unwrap();
        registry.remove(&id).unwrap();
        assert!(registry.get(&id).is_err());
    }
}
