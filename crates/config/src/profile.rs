use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type ProfileId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: ProfileId,
    pub name: String,
    pub description: String,
    pub config: HashMap<String, crate::config::ConfigValue>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl Profile {
    pub fn new(id: &str, name: &str) -> Self {
        let now = chrono::Utc::now();
        Self {
            id: id.to_string(),
            name: name.to_string(),
            description: String::new(),
            config: HashMap::new(),
            created_at: now,
            updated_at: now,
        }
    }

    pub fn with_description(mut self, desc: &str) -> Self {
        self.description = desc.to_string();
        self
    }

    pub fn set_config(&mut self, key: &str, value: crate::config::ConfigValue) {
        self.config.insert(key.to_string(), value);
        self.updated_at = chrono::Utc::now();
    }
}

#[derive(Debug, Clone)]
pub struct ProfileManager {
    profiles: std::sync::Arc<std::sync::RwLock<HashMap<ProfileId, Profile>>>,
    active: std::sync::Arc<std::sync::RwLock<Option<ProfileId>>>,
}

impl Default for ProfileManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ProfileManager {
    pub fn new() -> Self {
        Self {
            profiles: std::sync::Arc::new(std::sync::RwLock::new(HashMap::new())),
            active: std::sync::Arc::new(std::sync::RwLock::new(None)),
        }
    }

    pub fn create(&self, profile: Profile) -> crate::error::Result<ProfileId> {
        let mut profiles = self.profiles.write().map_err(|_| {
            crate::error::ConfigError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        let id = profile.id.clone();
        profiles.insert(id.clone(), profile);
        Ok(id)
    }

    pub fn get(&self, id: &ProfileId) -> crate::error::Result<Profile> {
        let profiles = self.profiles.read().map_err(|_| {
            crate::error::ConfigError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        profiles
            .get(id)
            .cloned()
            .ok_or_else(|| crate::error::ConfigError::ProfileNotFound(id.clone()))
    }

    pub fn list(&self) -> crate::error::Result<Vec<Profile>> {
        let profiles = self.profiles.read().map_err(|_| {
            crate::error::ConfigError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(profiles.values().cloned().collect())
    }

    pub fn set_active(&self, id: &ProfileId) -> crate::error::Result<()> {
        let profiles = self.profiles.read().map_err(|_| {
            crate::error::ConfigError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        if !profiles.contains_key(id) {
            return Err(crate::error::ConfigError::ProfileNotFound(id.clone()));
        }
        let mut active = self.active.write().map_err(|_| {
            crate::error::ConfigError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        *active = Some(id.clone());
        Ok(())
    }

    pub fn active(&self) -> crate::error::Result<Option<ProfileId>> {
        let active = self.active.read().map_err(|_| {
            crate::error::ConfigError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(active.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profile_creation() {
        let profile = Profile::new("p1", "Test Profile");
        assert_eq!(profile.name, "Test Profile");
    }

    #[test]
    fn test_profile_manager_create_and_get() {
        let manager = ProfileManager::new();
        let profile = Profile::new("p1", "Test");
        manager.create(profile).unwrap();
        let retrieved = manager.get(&"p1".to_string()).unwrap();
        assert_eq!(retrieved.name, "Test");
    }

    #[test]
    fn test_profile_manager_active() {
        let manager = ProfileManager::new();
        let profile = Profile::new("p1", "Test");
        manager.create(profile).unwrap();
        manager.set_active(&"p1".to_string()).unwrap();
        assert_eq!(manager.active().unwrap(), Some("p1".to_string()));
    }

    #[test]
    fn test_profile_manager_list() {
        let manager = ProfileManager::new();
        manager.create(Profile::new("p1", "Profile 1")).unwrap();
        manager.create(Profile::new("p2", "Profile 2")).unwrap();
        let list = manager.list().unwrap();
        assert_eq!(list.len(), 2);
    }
}
