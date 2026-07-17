use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

pub type GoalId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Frequency {
    Hourly,
    Daily,
    Weekly,
    Monthly,
    Custom(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum GoalStatus {
    Active,
    Paused,
    Completed,
    Abandoned,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandingGoal {
    pub id: GoalId,
    pub title: String,
    pub description: String,
    pub status: GoalStatus,
    pub frequency: Frequency,
    pub created_at: DateTime<Utc>,
    pub last_run: Option<DateTime<Utc>>,
    pub next_run: Option<DateTime<Utc>>,
    pub run_count: u64,
    pub success_count: u64,
    pub owner: String,
    pub metadata: HashMap<String, String>,
}

impl StandingGoal {
    pub fn new(title: &str, description: &str, frequency: Frequency, owner: &str) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::now_v7().to_string(),
            title: title.to_string(),
            description: description.to_string(),
            status: GoalStatus::Active,
            frequency,
            created_at: now,
            last_run: None,
            next_run: None,
            run_count: 0,
            success_count: 0,
            owner: owner.to_string(),
            metadata: HashMap::new(),
        }
    }

    pub fn pause(&mut self) {
        self.status = GoalStatus::Paused;
    }

    pub fn resume(&mut self) {
        self.status = GoalStatus::Active;
    }

    pub fn mark_completed(&mut self) {
        self.status = GoalStatus::Completed;
    }

    pub fn record_run(&mut self, success: bool) {
        self.last_run = Some(Utc::now());
        self.run_count += 1;
        if success {
            self.success_count += 1;
        }
    }

    pub fn success_rate(&self) -> f64 {
        if self.run_count == 0 {
            0.0
        } else {
            self.success_count as f64 / self.run_count as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_goal_creation() {
        let g = StandingGoal::new(
            "Daily Backup",
            "Backup data daily",
            Frequency::Daily,
            "gnani",
        );
        assert_eq!(g.title, "Daily Backup");
        assert_eq!(g.status, GoalStatus::Active);
        assert_eq!(g.run_count, 0);
    }

    #[test]
    fn test_goal_pause_resume() {
        let mut g = StandingGoal::new("Test", "Desc", Frequency::Hourly, "gnani");
        g.pause();
        assert_eq!(g.status, GoalStatus::Paused);
        g.resume();
        assert_eq!(g.status, GoalStatus::Active);
    }

    #[test]
    fn test_goal_record_run() {
        let mut g = StandingGoal::new("Test", "Desc", Frequency::Hourly, "gnani");
        g.record_run(true);
        g.record_run(false);
        g.record_run(true);
        assert_eq!(g.run_count, 3);
        assert_eq!(g.success_count, 2);
        assert_eq!(g.success_rate(), 2.0 / 3.0);
    }

    #[test]
    fn test_goal_success_rate_zero() {
        let g = StandingGoal::new("Test", "Desc", Frequency::Hourly, "gnani");
        assert_eq!(g.success_rate(), 0.0);
    }
}
