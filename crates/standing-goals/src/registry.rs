use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::error::{Result, StandingGoalError};
use crate::goal::{GoalId, GoalStatus, StandingGoal};

#[derive(Debug, Clone)]
pub struct GoalRegistry {
    goals: Arc<RwLock<HashMap<GoalId, StandingGoal>>>,
}

impl Default for GoalRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl GoalRegistry {
    pub fn new() -> Self {
        Self {
            goals: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn create(&self, goal: StandingGoal) -> Result<GoalId> {
        let mut goals = self.goals.write().map_err(|_| {
            StandingGoalError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        let id = goal.id.clone();
        goals.insert(id.clone(), goal);
        Ok(id)
    }

    pub fn get(&self, id: &GoalId) -> Result<StandingGoal> {
        let goals = self.goals.read().map_err(|_| {
            StandingGoalError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        goals
            .get(id)
            .cloned()
            .ok_or_else(|| StandingGoalError::NotFound(id.clone()))
    }

    pub fn list(&self) -> Result<Vec<StandingGoal>> {
        let goals = self.goals.read().map_err(|_| {
            StandingGoalError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(goals.values().cloned().collect())
    }

    pub fn list_by_status(&self, status: GoalStatus) -> Result<Vec<StandingGoal>> {
        let goals = self.goals.read().map_err(|_| {
            StandingGoalError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        Ok(goals
            .values()
            .filter(|g| g.status == status)
            .cloned()
            .collect())
    }

    pub fn update(&self, goal: StandingGoal) -> Result<()> {
        let mut goals = self.goals.write().map_err(|_| {
            StandingGoalError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        if !goals.contains_key(&goal.id) {
            return Err(StandingGoalError::NotFound(goal.id.clone()));
        }
        goals.insert(goal.id.clone(), goal);
        Ok(())
    }

    pub fn remove(&self, id: &GoalId) -> Result<()> {
        let mut goals = self.goals.write().map_err(|_| {
            StandingGoalError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                "poisoned lock",
            ))
        })?;
        goals
            .remove(id)
            .ok_or_else(|| StandingGoalError::NotFound(id.clone()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::Frequency;

    #[test]
    fn test_registry_create_and_get() {
        let registry = GoalRegistry::new();
        let g = StandingGoal::new("Test", "Desc", Frequency::Daily, "gnani");
        let id = registry.create(g.clone()).unwrap();
        let retrieved = registry.get(&id).unwrap();
        assert_eq!(retrieved.title, "Test");
    }

    #[test]
    fn test_registry_list_by_status() {
        let registry = GoalRegistry::new();
        let mut g1 = StandingGoal::new("Active", "Desc", Frequency::Daily, "gnani");
        let mut g2 = StandingGoal::new("Paused", "Desc", Frequency::Hourly, "gnani");
        g2.pause();
        registry.create(g1).unwrap();
        registry.create(g2).unwrap();

        let active = registry.list_by_status(GoalStatus::Active).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].title, "Active");
    }

    #[test]
    fn test_registry_remove() {
        let registry = GoalRegistry::new();
        let g = StandingGoal::new("Remove", "Desc", Frequency::Weekly, "gnani");
        let id = registry.create(g).unwrap();
        registry.remove(&id).unwrap();
        assert!(registry.get(&id).is_err());
    }
}
