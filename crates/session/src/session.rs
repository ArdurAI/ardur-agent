use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

pub type SessionId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SessionStatus {
    Active,
    Paused,
    Archived,
    Forked,
    Merged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    pub title: String,
    pub topic: String,
    pub model: String,
    pub provider: String,
    pub max_tokens: Option<usize>,
    pub temperature: Option<f32>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            title: "Untitled Session".to_string(),
            topic: "general".to_string(),
            model: "default".to_string(),
            provider: "default".to_string(),
            max_tokens: None,
            temperature: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub config: SessionConfig,
    pub status: SessionStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub message_count: u64,
    pub token_count: u64,
    pub parent_id: Option<SessionId>,
    pub fork_ids: Vec<SessionId>,
    pub metadata: HashMap<String, String>,
}

impl Session {
    pub fn new(config: SessionConfig) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::now_v7().to_string(),
            config,
            status: SessionStatus::Active,
            created_at: now,
            updated_at: now,
            last_active: now,
            message_count: 0,
            token_count: 0,
            parent_id: None,
            fork_ids: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    pub fn pause(&mut self) {
        self.status = SessionStatus::Paused;
        self.updated_at = Utc::now();
    }

    pub fn resume(&mut self) {
        self.status = SessionStatus::Active;
        self.last_active = Utc::now();
        self.updated_at = Utc::now();
    }

    pub fn archive(&mut self) {
        self.status = SessionStatus::Archived;
        self.updated_at = Utc::now();
    }

    pub fn fork(&self, new_config: SessionConfig) -> Self {
        let mut new_session = Self::new(new_config);
        new_session.parent_id = Some(self.id.clone());
        new_session
    }

    pub fn record_message(&mut self, tokens: u64) {
        self.message_count += 1;
        self.token_count += tokens;
        self.last_active = Utc::now();
        self.updated_at = Utc::now();
    }

    pub fn update_title(&mut self, title: &str) {
        self.config.title = title.to_string();
        self.updated_at = Utc::now();
    }

    pub fn update_topic(&mut self, topic: &str) {
        self.config.topic = topic.to_string();
        self.updated_at = Utc::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_creation() {
        let session = Session::new(SessionConfig::default());
        assert_eq!(session.config.title, "Untitled Session");
        assert_eq!(session.status, SessionStatus::Active);
        assert_eq!(session.message_count, 0);
    }

    #[test]
    fn test_session_pause_resume() {
        let mut session = Session::new(SessionConfig::default());
        session.pause();
        assert_eq!(session.status, SessionStatus::Paused);
        session.resume();
        assert_eq!(session.status, SessionStatus::Active);
    }

    #[test]
    fn test_session_archive() {
        let mut session = Session::new(SessionConfig::default());
        session.archive();
        assert_eq!(session.status, SessionStatus::Archived);
    }

    #[test]
    fn test_session_fork() {
        let session = Session::new(SessionConfig::default());
        let forked = session.fork(SessionConfig {
            title: "Forked".to_string(),
            ..SessionConfig::default()
        });
        assert_eq!(forked.config.title, "Forked");
        assert_eq!(forked.parent_id, Some(session.id.clone()));
    }

    #[test]
    fn test_session_record_message() {
        let mut session = Session::new(SessionConfig::default());
        session.record_message(100);
        session.record_message(50);
        assert_eq!(session.message_count, 2);
        assert_eq!(session.token_count, 150);
    }

    #[test]
    fn test_session_update_title() {
        let mut session = Session::new(SessionConfig::default());
        session.update_title("New Title");
        assert_eq!(session.config.title, "New Title");
    }

    #[test]
    fn test_session_update_topic() {
        let mut session = Session::new(SessionConfig::default());
        session.update_topic("coding");
        assert_eq!(session.config.topic, "coding");
    }
}
