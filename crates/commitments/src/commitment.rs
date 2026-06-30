use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

pub type CommitmentId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Priority {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CommitmentStatus {
    Declared,
    InProgress,
    DueSoon,
    Overdue,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commitment {
    pub id: CommitmentId,
    pub title: String,
    pub description: String,
    pub status: CommitmentStatus,
    pub priority: Priority,
    pub declared_at: DateTime<Utc>,
    pub due_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub owner: String,
    pub tags: Vec<String>,
    pub metadata: HashMap<String, String>,
}

impl Commitment {
    pub fn new(title: &str, description: &str, owner: &str) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::now_v7().to_string(),
            title: title.to_string(),
            description: description.to_string(),
            status: CommitmentStatus::Declared,
            priority: Priority::Medium,
            declared_at: now,
            due_at: None,
            completed_at: None,
            owner: owner.to_string(),
            tags: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    pub fn with_priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_due_date(mut self, due: DateTime<Utc>) -> Self {
        self.due_at = Some(due);
        self
    }

    pub fn start(&mut self) {
        self.status = CommitmentStatus::InProgress;
    }

    pub fn complete(&mut self) {
        self.status = CommitmentStatus::Completed;
        self.completed_at = Some(Utc::now());
    }

    pub fn cancel(&mut self) {
        self.status = CommitmentStatus::Cancelled;
    }

    pub fn check_due_status(&mut self) {
        if let Some(due) = self.due_at {
            let now = Utc::now();
            if self.status == CommitmentStatus::Declared
                || self.status == CommitmentStatus::InProgress
            {
                if now > due {
                    self.status = CommitmentStatus::Overdue;
                } else if (due - now).num_hours() < 24 {
                    self.status = CommitmentStatus::DueSoon;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_commitment_creation() {
        let c = Commitment::new("Test", "Description", "gnani");
        assert_eq!(c.title, "Test");
        assert_eq!(c.status, CommitmentStatus::Declared);
        assert_eq!(c.priority, Priority::Medium);
    }

    #[test]
    fn test_commitment_with_priority() {
        let c = Commitment::new("Test", "Desc", "gnani").with_priority(Priority::High);
        assert_eq!(c.priority, Priority::High);
    }

    #[test]
    fn test_commitment_lifecycle() {
        let mut c = Commitment::new("Test", "Desc", "gnani");
        c.start();
        assert_eq!(c.status, CommitmentStatus::InProgress);
        c.complete();
        assert_eq!(c.status, CommitmentStatus::Completed);
        assert!(c.completed_at.is_some());
    }

    #[test]
    fn test_commitment_cancel() {
        let mut c = Commitment::new("Test", "Desc", "gnani");
        c.cancel();
        assert_eq!(c.status, CommitmentStatus::Cancelled);
    }
}
