use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

pub type SkillId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SkillStatus {
    Draft,
    Active,
    Deprecated,
    Removed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub tags: Vec<String>,
    pub dependencies: Vec<String>,
    pub entry_point: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub id: SkillId,
    pub manifest: SkillManifest,
    pub status: SkillStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub usage_count: u64,
    pub metadata: HashMap<String, String>,
}

impl Skill {
    pub fn new(manifest: SkillManifest) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::now_v7().to_string(),
            manifest,
            status: SkillStatus::Draft,
            created_at: now,
            updated_at: now,
            usage_count: 0,
            metadata: HashMap::new(),
        }
    }

    pub fn activate(&mut self) {
        self.status = SkillStatus::Active;
        self.updated_at = Utc::now();
    }

    pub fn deprecate(&mut self) {
        self.status = SkillStatus::Deprecated;
        self.updated_at = Utc::now();
    }

    pub fn increment_usage(&mut self) {
        self.usage_count += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_manifest() -> SkillManifest {
        SkillManifest {
            name: "test-skill".to_string(),
            version: "1.0.0".to_string(),
            description: "A test skill".to_string(),
            author: "gnani".to_string(),
            tags: vec!["test".to_string()],
            dependencies: vec![],
            entry_point: "lib.rs".to_string(),
        }
    }

    #[test]
    fn test_skill_creation() {
        let skill = Skill::new(test_manifest());
        assert_eq!(skill.manifest.name, "test-skill");
        assert_eq!(skill.status, SkillStatus::Draft);
        assert_eq!(skill.usage_count, 0);
    }

    #[test]
    fn test_skill_activate() {
        let mut skill = Skill::new(test_manifest());
        skill.activate();
        assert_eq!(skill.status, SkillStatus::Active);
    }

    #[test]
    fn test_skill_deprecate() {
        let mut skill = Skill::new(test_manifest());
        skill.deprecate();
        assert_eq!(skill.status, SkillStatus::Deprecated);
    }

    #[test]
    fn test_skill_usage_count() {
        let mut skill = Skill::new(test_manifest());
        skill.increment_usage();
        skill.increment_usage();
        assert_eq!(skill.usage_count, 2);
    }
}
