use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

pub type TaskId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundTask {
    pub id: TaskId,
    pub name: String,
    pub description: String,
    pub status: TaskStatus,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub metadata: HashMap<String, String>,
}

impl BackgroundTask {
    pub fn new(name: &str, description: &str) -> Self {
        Self {
            id: Uuid::now_v7().to_string(),
            name: name.to_string(),
            description: description.to_string(),
            status: TaskStatus::Pending,
            created_at: Utc::now(),
            started_at: None,
            completed_at: None,
            result: None,
            error: None,
            metadata: HashMap::new(),
        }
    }

    pub fn start(&mut self) {
        self.status = TaskStatus::Running;
        self.started_at = Some(Utc::now());
    }

    pub fn complete(&mut self, result: &str) {
        self.status = TaskStatus::Completed;
        self.completed_at = Some(Utc::now());
        self.result = Some(result.to_string());
    }

    pub fn fail(&mut self, error: &str) {
        self.status = TaskStatus::Failed;
        self.completed_at = Some(Utc::now());
        self.error = Some(error.to_string());
    }

    pub fn cancel(&mut self) {
        self.status = TaskStatus::Cancelled;
        self.completed_at = Some(Utc::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_creation() {
        let task = BackgroundTask::new("test", "A test task");
        assert_eq!(task.name, "test");
        assert_eq!(task.status, TaskStatus::Pending);
    }

    #[test]
    fn test_task_start() {
        let mut task = BackgroundTask::new("test", "Desc");
        task.start();
        assert_eq!(task.status, TaskStatus::Running);
        assert!(task.started_at.is_some());
    }

    #[test]
    fn test_task_complete() {
        let mut task = BackgroundTask::new("test", "Desc");
        task.start();
        task.complete("success");
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.result, Some("success".to_string()));
    }

    #[test]
    fn test_task_fail() {
        let mut task = BackgroundTask::new("test", "Desc");
        task.fail("error occurred");
        assert_eq!(task.status, TaskStatus::Failed);
        assert_eq!(task.error, Some("error occurred".to_string()));
    }

    #[test]
    fn test_task_cancel() {
        let mut task = BackgroundTask::new("test", "Desc");
        task.cancel();
        assert_eq!(task.status, TaskStatus::Cancelled);
    }
}
