use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::error::{CuratorError, Result};
use crate::skill::{Skill, SkillId, SkillStatus};

#[derive(Debug, Clone)]
pub struct SkillRegistry {
    skills: Arc<RwLock<HashMap<SkillId, Skill>>>,
    name_index: Arc<RwLock<HashMap<String, SkillId>>>,
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: Arc::new(RwLock::new(HashMap::new())),
            name_index: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn register(&self, skill: Skill) -> Result<SkillId> {
        let mut skills = self.skills.write().map_err(|_| {
            CuratorError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        let mut name_index = self.name_index.write().map_err(|_| {
            CuratorError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;

        if name_index.contains_key(&skill.manifest.name) {
            return Err(CuratorError::SkillAlreadyExists(skill.manifest.name.clone()));
        }

        let id = skill.id.clone();
        name_index.insert(skill.manifest.name.clone(), id.clone());
        skills.insert(id.clone(), skill);
        Ok(id)
    }

    pub fn get(&self, id: &SkillId) -> Result<Skill> {
        let skills = self.skills.read().map_err(|_| {
            CuratorError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        skills.get(id).cloned().ok_or_else(|| CuratorError::SkillNotFound(id.clone()))
    }

    pub fn get_by_name(&self, name: &str) -> Result<Skill> {
        let name_index = self.name_index.read().map_err(|_| {
            CuratorError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        let id = name_index.get(name).ok_or_else(|| CuratorError::SkillNotFound(name.to_string()))?;
        self.get(id)
    }

    pub fn list(&self) -> Result<Vec<Skill>> {
        let skills = self.skills.read().map_err(|_| {
            CuratorError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(skills.values().cloned().collect())
    }

    pub fn list_by_status(&self, status: SkillStatus) -> Result<Vec<Skill>> {
        let skills = self.skills.read().map_err(|_| {
            CuratorError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(skills.values().filter(|s| s.status == status).cloned().collect())
    }

    pub fn update(&self, skill: Skill) -> Result<()> {
        let mut skills = self.skills.write().map_err(|_| {
            CuratorError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        if !skills.contains_key(&skill.id) {
            return Err(CuratorError::SkillNotFound(skill.id.clone()));
        }
        skills.insert(skill.id.clone(), skill);
        Ok(())
    }

    pub fn remove(&self, id: &SkillId) -> Result<()> {
        let mut skills = self.skills.write().map_err(|_| {
            CuratorError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        let mut name_index = self.name_index.write().map_err(|_| {
            CuratorError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;

        let skill = skills.remove(id).ok_or_else(|| CuratorError::SkillNotFound(id.clone()))?;
        name_index.remove(&skill.manifest.name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::SkillManifest;

    fn test_skill(name: &str) -> Skill {
        Skill::new(SkillManifest {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            description: "test".to_string(),
            author: "gnani".to_string(),
            tags: vec![],
            dependencies: vec![],
            entry_point: "lib.rs".to_string(),
        })
    }

    #[test]
    fn test_registry_register_and_get() {
        let registry = SkillRegistry::new();
        let skill = test_skill("test-skill");
        let id = registry.register(skill.clone()).unwrap();
        let retrieved = registry.get(&id).unwrap();
        assert_eq!(retrieved.manifest.name, "test-skill");
    }

    #[test]
    fn test_registry_get_by_name() {
        let registry = SkillRegistry::new();
        let skill = test_skill("my-skill");
        registry.register(skill).unwrap();
        let retrieved = registry.get_by_name("my-skill").unwrap();
        assert_eq!(retrieved.manifest.name, "my-skill");
    }

    #[test]
    fn test_registry_duplicate_name() {
        let registry = SkillRegistry::new();
        let skill1 = test_skill("dup");
        let skill2 = test_skill("dup");
        registry.register(skill1).unwrap();
        assert!(registry.register(skill2).is_err());
    }

    #[test]
    fn test_registry_list_by_status() {
        let registry = SkillRegistry::new();
        let mut active = test_skill("active");
        active.activate();
        let draft = test_skill("draft");
        registry.register(active).unwrap();
        registry.register(draft).unwrap();

        let active_list = registry.list_by_status(SkillStatus::Active).unwrap();
        assert_eq!(active_list.len(), 1);
        assert_eq!(active_list[0].manifest.name, "active");
    }

    #[test]
    fn test_registry_remove() {
        let registry = SkillRegistry::new();
        let skill = test_skill("remove-me");
        let id = registry.register(skill).unwrap();
        registry.remove(&id).unwrap();
        assert!(registry.get(&id).is_err());
        assert!(registry.get_by_name("remove-me").is_err());
    }
}
