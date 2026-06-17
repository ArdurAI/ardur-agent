use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type MessageId = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MessageType {
    Text,
    Image,
    Audio,
    Video,
    File,
    Command,
    Event,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MessageStatus {
    Pending,
    Sent,
    Delivered,
    Read,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub channel_id: String,
    pub sender: String,
    pub recipient: Option<String>,
    pub message_type: MessageType,
    pub content: String,
    pub status: MessageStatus,
    pub created_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub metadata: HashMap<String, String>,
}

impl Message {
    pub fn new(channel_id: &str, sender: &str, content: &str, message_type: MessageType) -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            channel_id: channel_id.to_string(),
            sender: sender.to_string(),
            recipient: None,
            message_type,
            content: content.to_string(),
            status: MessageStatus::Pending,
            created_at: Utc::now(),
            delivered_at: None,
            metadata: HashMap::new(),
        }
    }

    pub fn with_recipient(mut self, recipient: &str) -> Self {
        self.recipient = Some(recipient.to_string());
        self
    }

    pub fn mark_sent(&mut self) {
        self.status = MessageStatus::Sent;
    }

    pub fn mark_delivered(&mut self) {
        self.status = MessageStatus::Delivered;
        self.delivered_at = Some(Utc::now());
    }

    pub fn mark_read(&mut self) {
        self.status = MessageStatus::Read;
    }

    pub fn mark_failed(&mut self) {
        self.status = MessageStatus::Failed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_creation() {
        let msg = Message::new("ch-1", "user1", "Hello", MessageType::Text);
        assert_eq!(msg.channel_id, "ch-1");
        assert_eq!(msg.sender, "user1");
        assert_eq!(msg.status, MessageStatus::Pending);
    }

    #[test]
    fn test_message_with_recipient() {
        let msg = Message::new("ch-1", "user1", "Hello", MessageType::Text).with_recipient("user2");
        assert_eq!(msg.recipient, Some("user2".to_string()));
    }

    #[test]
    fn test_message_lifecycle() {
        let mut msg = Message::new("ch-1", "user1", "Hello", MessageType::Text);
        msg.mark_sent();
        assert_eq!(msg.status, MessageStatus::Sent);
        msg.mark_delivered();
        assert_eq!(msg.status, MessageStatus::Delivered);
        assert!(msg.delivered_at.is_some());
        msg.mark_read();
        assert_eq!(msg.status, MessageStatus::Read);
    }
}
