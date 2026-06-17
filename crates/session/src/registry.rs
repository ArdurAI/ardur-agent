use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::error::{Result, SessionError};
use crate::session::{Session, SessionId, SessionStatus};

#[derive(Debug, Clone)]
pub struct SessionRegistry {
    sessions: Arc<RwLock<HashMap<SessionId, Session>>>,
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn create(&self, session: Session) -> Result<SessionId> {
        let mut sessions = self.sessions.write().map_err(|_| {
            SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        let id = session.id.clone();
        sessions.insert(id.clone(), session);
        Ok(id)
    }

    pub fn get(&self, id: &SessionId) -> Result<Session> {
        let sessions = self.sessions.read().map_err(|_| {
            SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        sessions
            .get(id)
            .cloned()
            .ok_or_else(|| SessionError::NotFound(id.clone()))
    }

    pub fn list(&self) -> Result<Vec<Session>> {
        let sessions = self.sessions.read().map_err(|_| {
            SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(sessions.values().cloned().collect())
    }

    pub fn list_by_status(&self, status: SessionStatus) -> Result<Vec<Session>> {
        let sessions = self.sessions.read().map_err(|_| {
            SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(sessions
            .values()
            .filter(|s| s.status == status)
            .cloned()
            .collect())
    }

    pub fn update(&self, session: Session) -> Result<()> {
        let mut sessions = self.sessions.write().map_err(|_| {
            SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        if !sessions.contains_key(&session.id) {
            return Err(SessionError::NotFound(session.id.clone()));
        }
        sessions.insert(session.id.clone(), session);
        Ok(())
    }

    pub fn delete(&self, id: &SessionId) -> Result<()> {
        let mut sessions = self.sessions.write().map_err(|_| {
            SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        sessions
            .remove(id)
            .ok_or_else(|| SessionError::NotFound(id.clone()))?;
        Ok(())
    }

    pub fn prune_inactive(&self, before: chrono::DateTime<chrono::Utc>) -> Result<Vec<SessionId>> {
        let mut sessions = self.sessions.write().map_err(|_| {
            SessionError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        let mut pruned = Vec::new();
        sessions.retain(|id, session| {
            if session.last_active < before && session.status == SessionStatus::Paused {
                pruned.push(id.clone());
                false
            } else {
                true
            }
        });
        Ok(pruned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionConfig;

    #[test]
    fn test_registry_create_and_get() {
        let registry = SessionRegistry::new();
        let session = Session::new(SessionConfig::default());
        let id = registry.create(session.clone()).unwrap();
        let retrieved = registry.get(&id).unwrap();
        assert_eq!(retrieved.config.title, "Untitled Session");
    }

    #[test]
    fn test_registry_list_by_status() {
        let registry = SessionRegistry::new();
        let mut s1 = Session::new(SessionConfig::default());
        s1.pause();
        let s2 = Session::new(SessionConfig::default());
        registry.create(s1).unwrap();
        registry.create(s2).unwrap();

        let paused = registry.list_by_status(SessionStatus::Paused).unwrap();
        assert_eq!(paused.len(), 1);
    }

    #[test]
    fn test_registry_delete() {
        let registry = SessionRegistry::new();
        let session = Session::new(SessionConfig::default());
        let id = registry.create(session).unwrap();
        registry.delete(&id).unwrap();
        assert!(registry.get(&id).is_err());
    }

    #[test]
    fn test_registry_prune_inactive() {
        let registry = SessionRegistry::new();
        let mut old_session = Session::new(SessionConfig::default());
        old_session.pause();
        // Manually set last_active to old time
        old_session.last_active = chrono::Utc::now() - chrono::Duration::days(30);
        let id = registry.create(old_session).unwrap();

        let pruned = registry
            .prune_inactive(chrono::Utc::now() - chrono::Duration::days(7))
            .unwrap();
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0], id);
        assert!(registry.get(&id).is_err());
    }
}
